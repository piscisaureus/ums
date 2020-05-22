// I think there's nothing wrong with this pattern, so silence clippy about it.
#![allow(clippy::try_err)]

mod win_result;
mod winapi_extra;

use crate::win_result::WinError;
use crate::win_result::WinResult;
use crate::winapi_extra::*;

use scopeguard::guard;
use scopeguard::ScopeGuard;

use std::cell::Cell;
use std::collections::VecDeque;
use std::fmt::Debug;
use std::future::Future;
use std::io;
use std::iter::repeat;
use std::mem::align_of;
use std::mem::replace;
use std::mem::size_of;
use std::mem::size_of_val;
use std::mem::transmute;
use std::mem::MaybeUninit;
use std::pin::Pin;
use std::ptr;
use std::ptr::null_mut;
use std::ptr::NonNull;

use winapi::shared::basetsd::*;
use winapi::shared::minwindef::*;
use winapi::shared::winerror::*;
use winapi::um::handleapi::*;
use winapi::um::processthreadsapi::*;
use winapi::um::synchapi::*;
use winapi::um::winbase::*;
use winapi::um::winnt::*;

enum DelegationState {
  Queued,
  Delegated {
    ums_thread_context: *mut UMS_CONTEXT,
  },
}

enum NotificationMode {
  ExecuteThread {
    ums_thread_context: *mut UMS_CONTEXT,
  },
}

trait Job {
  fn main(&mut self) {}
  fn on_blocking(&mut self) -> Option<*mut UMS_CONTEXT> {
    None
  }
  fn on_complete(&mut self) -> Option<*mut UMS_CONTEXT> {
    None
  }
}

#[derive(Copy, Clone)]
enum ThreadEvent {
  None,
  // Thread notifies scheduler of being available.
  Startup,
  // Thread asks scheduler to delegate a job.
  RequestJob(*mut dyn Job),
  // Scheduler assigns job to thread.
  JobAssigned(*mut dyn Job),
  // Job notifies scheduler of job completion.
  JobFinished(*mut dyn Job),
}

impl Default for ThreadEvent {
  fn default() -> Self {
    Self::None
  }
}

struct ThreadState {
  ums_thread_context: *mut UMS_CONTEXT,
  event: Cell<ThreadEvent>,
}

impl ThreadState {
  pub fn new() -> Self {
    Self {
      ums_thread_context: unsafe { GetCurrentUmsThread() }.unwrap(),
      event: Default::default(),
    }
  }

  pub fn set_event(&mut self, event: ThreadEvent) {
    self.event = Cell::new(event);
  }

  pub fn yield_event(&mut self, event: ThreadEvent) {
    self.set_event(event);
    let param = self as *mut _ as *mut VOID;
    unsafe { UmsThreadYield(param) }.unwrap();
  }

  pub fn get_event(&self) -> ThreadEvent {
    self.event.get()
  }
}

unsafe extern "system" fn thread_main_1(_: *mut VOID) -> DWORD {
  let mut state = ThreadState::new();
  set_ums_thread_info::<*mut ThreadState, io::Error>(
    state.ums_thread_context,
    UmsThreadUserContext,
    &mut state,
  )
  .unwrap();

  state.yield_event(ThreadEvent::Startup);
  loop {
    match state.get_event() {
      ThreadEvent::JobAssigned(job) => {
        unsafe { (*job).main() };
        state.yield_event(ThreadEvent::JobFinished(job))
      }
      _ => unreachable!(),
    }
  }
}

struct SchedulerState {
  pub queued_jobs: VecDeque<*mut dyn Job>,
  pub idle_threads: VecDeque<*mut UMS_CONTEXT>,
  pub runnable_threads: VecDeque<*mut UMS_CONTEXT>,
  pub thread_count_starting: usize,
  pub thread_count_total: usize,
  pub last_executed_thread: Option<NonNull<ThreadState>>,
  pub ums_completion_list: *mut UMS_COMPLETION_LIST,
}

unsafe impl Send for SchedulerState {}
unsafe impl Sync for SchedulerState {}

impl SchedulerState {
  pub fn new(ums_completion_list: *mut UMS_COMPLETION_LIST) -> Self {
    Self {
      queued_jobs: VecDeque::new(),
      idle_threads: VecDeque::new(),
      runnable_threads: VecDeque::new(),
      thread_count_starting: 0,
      thread_count_total: 0,
      last_executed_thread: None,
      ums_completion_list,
    }
  }
}

#[derive(Debug)]
enum ThreadBlockedCause {
  Trap,
  SysCall,
}

#[derive(Debug)]
enum ThreadReadyCause {
  Yield { param: *mut VOID },
  Busy,
  Suspended,
  Terminated,
  ExecuteFailed { error: io::Error },
}

#[derive(Debug)]
#[allow(dead_code)] // False positive.
enum SchedulerEntryCause {
  Startup {
    param: *mut VOID,
  },
  ThreadBlocked {
    cause: ThreadBlockedCause,
  },
  ThreadReady {
    ums_thread_context: *mut UMS_CONTEXT,
    cause: ThreadReadyCause,
  },
}

impl SchedulerEntryCause {
  pub fn from_scheduler_entry_args(
    reason: UMS_SCHEDULER_REASON,
    activation_payload: ULONG_PTR,
    scheduler_param: *mut VOID,
  ) -> Self {
    #[allow(non_upper_case_globals)]
    match reason {
      UmsSchedulerStartup => Self::Startup {
        param: scheduler_param,
      },
      UmsSchedulerThreadBlocked if activation_payload == 0 => {
        Self::ThreadBlocked {
          cause: ThreadBlockedCause::Trap,
        }
      }
      UmsSchedulerThreadBlocked if activation_payload == 1 => {
        Self::ThreadBlocked {
          cause: ThreadBlockedCause::SysCall,
        }
      }
      UmsSchedulerThreadYield => Self::ThreadReady {
        cause: ThreadReadyCause::Yield {
          param: scheduler_param,
        },
        ums_thread_context: activation_payload as *mut UMS_CONTEXT,
      },
      _ => unreachable!(),
    }
  }

  pub fn from_execute_error(
    ums_thread_context: *mut UMS_CONTEXT,
    error: DWORD,
  ) -> Self {
    let cause = match error {
      ERROR_RETRY => ThreadReadyCause::Busy,
      ERROR_SUCCESS => unreachable!(),
      _ if Self::get_ums_thread_flag(
        ums_thread_context,
        UmsThreadIsTerminated,
      ) =>
      {
        ThreadReadyCause::Terminated
      }
      _ if Self::get_ums_thread_flag(
        ums_thread_context,
        UmsThreadIsSuspended,
      ) =>
      {
        ThreadReadyCause::Suspended
      }
      error => {
        let error = io::Error::from_raw_os_error(error as i32);
        ThreadReadyCause::ExecuteFailed { error }
      }
    };
    Self::ThreadReady {
      ums_thread_context,
      cause,
    }
  }

  fn get_ums_thread_flag(
    ums_thread_context: *mut UMS_CONTEXT,
    ums_thread_info_class: UMS_THREAD_INFO_CLASS,
  ) -> bool {
    get_ums_thread_info::<BOOLEAN, ()>(
      ums_thread_context,
      ums_thread_info_class,
    )
    .map(|flag| flag != 0)
    .unwrap_or(false)
  }
}

extern "system" fn scheduler_entry(
  reason: UMS_SCHEDULER_REASON,
  activation_payload: ULONG_PTR,
  scheduler_param: *mut VOID,
) {
  // The `ExecuteUmsThread()` does not return if succesful, and certainly Rust
  // does not expect that, so we are at risk of skipping Drop handlers.
  // Therefore, most of the actual work is done in `scheduler_entry_impl()`.
  // We just have to be sure that no variables in this function require cleanup.
  let mut cause = SchedulerEntryCause::from_scheduler_entry_args(
    reason,
    activation_payload,
    scheduler_param,
  );
  while let Some(ums_thread_context) = scheduler_entry_impl(cause) {
    // `ExecuteUmsThread()` does not return unless it fails.
    let error = unsafe { ExecuteUmsThread(ums_thread_context) }.unwrap_err();
    cause = SchedulerEntryCause::from_execute_error(ums_thread_context, error);
  }
}

fn scheduler_entry_impl(
  cause: SchedulerEntryCause,
) -> Option<*mut UMS_CONTEXT> {
  use SchedulerEntryCause::*;

  eprintln!("S: {:?}", cause);

  let state = unsafe {
    GetCurrentUmsThread()
      .result()
      .and_then(|ums_thread_context| match cause {
        Startup { param } => {
          let state = param as *mut SchedulerState;
          set_ums_thread_info(ums_thread_context, UmsThreadUserContext, state)
            .map(|_| state)
        }
        _ => get_ums_thread_info(ums_thread_context, UmsThreadUserContext),
      })
      .map(|state| &mut *state)
      .unwrap()
  };

  let next_thread = match cause {
    Startup { .. } => {
      for _ in 0..4 {
        let _ = create_ums_thread(state.ums_completion_list).unwrap();
        state.thread_count_total += 1;
      }
      None
    }
    ThreadBlocked { .. } => state
      .last_executed_thread
      .map(|thread_state| unsafe { thread_state.as_ref().get_event() })
      .and_then(|thread_event| match thread_event {
        ThreadEvent::JobAssigned(job) => Some(job),
        _ => None,
      })
      .and_then(|job| unsafe { (*job).on_blocking() }),
    ThreadReady {
      ums_thread_context,
      cause: ThreadReadyCause::Yield { param },
    } => {
      let thread_state = unsafe { &*(param as *mut ThreadState) };
      debug_assert_eq!(ums_thread_context, thread_state.ums_thread_context);
      None
    }
    ThreadReady {
      ums_thread_context,
      cause: ThreadReadyCause::Terminated,
    } => {
      unsafe { DeleteUmsThreadContext(ums_thread_context) }.unwrap();
      state.thread_count_total -= 1;
      None
    }
    ThreadReady {
      ums_thread_context,
      cause: ThreadReadyCause::ExecuteFailed { error },
    } => panic!("ExecuteUmsThread({:?}): {}", ums_thread_context, error),
    ThreadReady {
      ums_thread_context, ..
    } => {
      state.runnable_threads.push_back(ums_thread_context);
      None
    }
  };
  state.last_executed_thread.take();

  if let Some(ums_thread_context) = next_thread {
    return Some(ums_thread_context);
  }

  if state.queued_jobs.is_empty() && state.thread_count_total == 0 {
    return None;
  }

  loop {
    eprintln!("selecting. ready# = {}", state.runnable_threads.len());
    match state.runnable_threads.pop_front() {
      Some(ums_thread_context) => {
        eprintln!("scheduling: {:X?}", ums_thread_context);
        break Some(ums_thread_context);
      }
      None => {
        eprintln!("waiting for ready threads");
        let mut ums_thread_context_list_head: *mut UMS_CONTEXT = null_mut();
        unsafe {
          DequeueUmsCompletionListItems(
            state.ums_completion_list,
            INFINITE,
            &mut ums_thread_context_list_head,
          )
        }
        .unwrap();
        while !(ums_thread_context_list_head.is_null()) {
          eprintln!("w<r");
          state
            .runnable_threads
            .push_back(ums_thread_context_list_head);
          ums_thread_context_list_head =
            unsafe { GetNextUmsListItem(ums_thread_context_list_head) };
        }
      }
    }
  }
}

pub fn run_ums_scheduler() -> io::Result<()> {
  unsafe {
    let mut ums_completion_list: *mut UMS_COMPLETION_LIST = null_mut();
    CreateUmsCompletionList(&mut ums_completion_list).result()?;
    assert!(!ums_completion_list.is_null());
    let ums_completion_list = guard(ums_completion_list, |l| {
      DeleteUmsCompletionList(l).unwrap();
    });

    let mut scheduler_state = SchedulerState::new(*ums_completion_list);

    let mut scheduler_startup_info = UMS_SCHEDULER_STARTUP_INFO {
      UmsVersion: UMS_VERSION,
      CompletionList: *ums_completion_list,
      SchedulerProc: Some(scheduler_entry),
      SchedulerParam: &mut scheduler_state as *mut _ as *mut VOID,
    };
    EnterUmsSchedulingMode(&mut scheduler_startup_info).result()
  }
}

unsafe extern "system" fn thread_main(_param: *mut VOID) -> DWORD {
  for _ in 0..10 {
    println!("In thread {:?}", GetCurrentUmsThread());
    Sleep(1000);
  }
  123
}

fn create_ums_thread(
  ums_completion_list: *mut UMS_COMPLETION_LIST,
) -> io::Result<*mut UMS_CONTEXT> {
  unsafe {
    let mut ums_thread_context = null_mut();
    CreateUmsThreadContext(&mut ums_thread_context).result()?;
    let ums_thread_context = guard(ums_thread_context, |c| {
      DeleteUmsThreadContext(c).unwrap();
    });

    let mut attr_list_size = 0;
    InitializeProcThreadAttributeList(null_mut(), 1, 0, &mut attr_list_size)
      .unwrap_err::<()>();
    assert_ne!(attr_list_size, 0);

    let mut attr_list = repeat(0u8).take(attr_list_size).collect::<Box<[u8]>>();
    let attr_list: *mut PROC_THREAD_ATTRIBUTE_LIST =
      cast_aligned(attr_list.as_mut_ptr());

    InitializeProcThreadAttributeList(attr_list, 1, 0, &mut attr_list_size)
      .result()?;
    let attr_list = guard(attr_list, |p| DeleteProcThreadAttributeList(p));

    let attr = UMS_CREATE_THREAD_ATTRIBUTES {
      UmsVersion: UMS_VERSION,
      UmsContext: *ums_thread_context,
      UmsCompletionList: ums_completion_list,
    };

    UpdateProcThreadAttribute(
      *attr_list,
      0,
      PROC_THREAD_ATTRIBUTE_UMS_THREAD,
      cast_mut_void(&attr),
      size_of_val(&attr),
      null_mut(),
      null_mut(),
    )
    .result()?;

    let thread_handle = CreateRemoteThreadEx(
      GetCurrentProcess(),
      null_mut(),
      1 << 20,
      Some(thread_main),
      null_mut(),
      STACK_SIZE_PARAM_IS_A_RESERVATION,
      *attr_list,
      null_mut(),
    )
    .result()?;
    CloseHandle(thread_handle).result()?;

    Ok(ScopeGuard::into_inner(ums_thread_context))
  }
}

fn get_ums_thread_info<T: Copy + 'static, E: WinError>(
  ums_thread_context: *mut UMS_CONTEXT,
  ums_thread_info_class: UMS_THREAD_INFO_CLASS,
) -> Result<T, E> {
  let mut info = MaybeUninit::<T>::uninit();
  let info_size_in = size_of::<T>() as ULONG;
  let mut info_size_out = MaybeUninit::<ULONG>::uninit();
  unsafe {
    QueryUmsThreadInformation(
      ums_thread_context,
      ums_thread_info_class,
      info.as_mut_ptr() as *mut VOID,
      info_size_in,
      info_size_out.as_mut_ptr(),
    )
    .ok_or_else(E::get_last_error)
    .map(move |_| assert_eq!(info_size_in, info_size_out.assume_init()))
    .map(move |_| info.assume_init())
  }
}

fn set_ums_thread_info<T: Copy + 'static, E: WinError>(
  ums_thread_context: *mut UMS_CONTEXT,
  ums_thread_info_class: UMS_THREAD_INFO_CLASS,
  info: T,
) -> Result<(), E> {
  unsafe {
    SetUmsThreadInformation(
      ums_thread_context,
      ums_thread_info_class,
      &info as *const T as *mut T as *mut VOID,
      size_of::<T>() as ULONG,
    )
    .ok_or_else(E::get_last_error)
  }
}

fn cast_aligned<T, U>(p: *mut T) -> *mut U {
  let address = p as usize;
  let mask = align_of::<U>() - 1;
  assert_eq!(address & mask, 0);
  address as *mut U
}

#[allow(dead_code)]
fn cast_mut_void<T>(p: *const T) -> *mut VOID {
  p as *mut T as *mut VOID
}
