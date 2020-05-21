// I think there's nothing wrong with this pattern, so silence clippy about it.
#![allow(clippy::try_err)]

mod win_result;
mod winapi_extra;

use crate::win_result::WinResult;
use crate::winapi_extra::*;
use derive_deref::*;
use scopeguard::guard;
use scopeguard::ScopeGuard;
use std::cell::RefCell;
use std::cell::RefMut;
use std::collections::VecDeque;
use std::fmt::Debug;
use std::io;
use std::iter::repeat;
use std::mem::align_of;
use std::mem::size_of_val;
use std::ptr::null_mut;
use winapi::shared::basetsd::*;
use winapi::shared::minwindef::*;
use winapi::shared::winerror::*;
use winapi::um::handleapi::*;
use winapi::um::processthreadsapi::*;
use winapi::um::synchapi::*;
use winapi::um::winbase::*;
use winapi::um::winnt::*;

static STATE: StaticState = StaticState(RefCell::new(None));

#[derive(Deref, DerefMut)]
struct StaticState(RefCell<Option<UmsSchedulerState>>);

unsafe impl Send for StaticState {}
unsafe impl Sync for StaticState {}

struct UmsSchedulerState {
  pub completion_list: *mut UMS_COMPLETION_LIST,
  pub runnable_threads: VecDeque<*mut UMS_CONTEXT>,
  pub thread_count: usize,
}

unsafe impl Send for UmsSchedulerState {}
unsafe impl Sync for UmsSchedulerState {}

impl UmsSchedulerState {
  pub fn new(completion_list: *mut UMS_COMPLETION_LIST) -> Self {
    Self {
      completion_list,
      runnable_threads: VecDeque::new(),
      thread_count: 0,
    }
  }

  pub fn init(completion_list: *mut UMS_COMPLETION_LIST) {
    let state = Self::new(completion_list);
    let mut ref_mut = STATE.borrow_mut();
    let prev = ref_mut.replace(state);
    assert!(prev.is_none());
  }

  pub fn get<'a>() -> RefMut<'a, Self> {
    let ref_mut = STATE.borrow_mut();
    RefMut::map(ref_mut, |r| r.as_mut().unwrap())
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
    thread_context: *mut UMS_CONTEXT,
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
        thread_context: activation_payload as *mut UMS_CONTEXT,
      },
      _ => unreachable!(),
    }
  }

  pub fn from_execute_error(
    thread_context: *mut UMS_CONTEXT,
    error: DWORD,
  ) -> Self {
    let cause = match error {
      ERROR_RETRY => ThreadReadyCause::Busy,
      ERROR_SUCCESS => unreachable!(),
      _ if Self::get_thread_flag(thread_context, UmsThreadIsTerminated) => {
        ThreadReadyCause::Terminated
      }
      _ if Self::get_thread_flag(thread_context, UmsThreadIsSuspended) => {
        ThreadReadyCause::Suspended
      }
      error => {
        let error = io::Error::from_raw_os_error(error as i32);
        ThreadReadyCause::ExecuteFailed { error }
      }
    };
    Self::ThreadReady {
      thread_context,
      cause,
    }
  }

  fn get_thread_flag(
    thread_context: *mut UMS_CONTEXT,
    thread_info_class: UMS_THREAD_INFO_CLASS,
  ) -> bool {
    let mut flag: BOOLEAN = 0;
    let mut return_length: ULONG = 0;
    unsafe {
      QueryUmsThreadInformation(
        thread_context,
        thread_info_class,
        &mut flag as *mut _ as *mut VOID,
        size_of_val(&flag) as ULONG,
        &mut return_length,
      )
    }
    .ok()
    .map(|_| assert_eq!(return_length as usize, size_of_val(&flag)))
    .map(|_| flag != 0)
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
  while let Some(thread_context) = scheduler_entry_impl(cause) {
    // `ExecuteUmsThread()` does not return unless it fails.
    let error = unsafe { ExecuteUmsThread(thread_context) }.unwrap_err();
    cause = SchedulerEntryCause::from_execute_error(thread_context, error);
  }
}

fn scheduler_entry_impl(
  cause: SchedulerEntryCause,
) -> Option<*mut UMS_CONTEXT> {
  eprintln!("S: {:?}", cause);
  let mut state = UmsSchedulerState::get();

  use SchedulerEntryCause::*;
  match cause {
    Startup { .. } => {
      for _ in 0..4 {
        let _ = create_ums_thread(state.completion_list).unwrap();
        state.thread_count += 1;
      }
    }
    ThreadReady {
      thread_context,
      cause: ThreadReadyCause::Terminated,
    } => {
      unsafe { DeleteUmsThreadContext(thread_context) }.unwrap();
      state.thread_count -= 1;
      if state.thread_count == 0 {
        return None;
      }
    }
    ThreadReady {
      thread_context,
      cause: ThreadReadyCause::ExecuteFailed { error },
    } => panic!("Failed to execute thread {:?}: {}", thread_context, error),
    ThreadReady { thread_context, .. } => {
      state.runnable_threads.push_back(thread_context)
    }
    _ => {}
  };

  loop {
    eprintln!("selecting. ready# = {}", state.runnable_threads.len());
    match state.runnable_threads.pop_front() {
      Some(thread_context) => {
        eprintln!("scheduling: {:X?}", thread_context);
        break Some(thread_context);
      }
      None => {
        eprintln!("waiting for ready threads");
        let mut list_item: *mut UMS_CONTEXT = null_mut();
        unsafe {
          DequeueUmsCompletionListItems(
            state.completion_list,
            INFINITE,
            &mut list_item,
          )
        }
        .unwrap();
        while !(list_item.is_null()) {
          eprintln!("w<r");
          state.runnable_threads.push_back(list_item);
          list_item = unsafe { GetNextUmsListItem(list_item) };
        }
      }
    }
  }
}

pub fn run_ums_scheduler() -> io::Result<()> {
  unsafe {
    let mut completion_list: *mut UMS_COMPLETION_LIST = null_mut();
    CreateUmsCompletionList(&mut completion_list).result()?;
    assert!(!completion_list.is_null());
    let _completion_list_guard = guard(completion_list, |p| {
      DeleteUmsCompletionList(p).unwrap();
    });

    UmsSchedulerState::init(completion_list);
    assert_eq!(UmsSchedulerState::get().completion_list, completion_list);

    let mut scheduler_startup_info = UMS_SCHEDULER_STARTUP_INFO {
      UmsVersion: UMS_VERSION,
      CompletionList: completion_list,
      SchedulerProc: Some(scheduler_entry),
      SchedulerParam: null_mut(),
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
  completion_list: *mut UMS_COMPLETION_LIST,
) -> io::Result<*mut UMS_CONTEXT> {
  unsafe {
    let mut thread_context = null_mut();
    CreateUmsThreadContext(&mut thread_context).result()?;
    let thread_context = guard(thread_context, |c| {
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
      UmsContext: *thread_context,
      UmsCompletionList: completion_list,
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

    Ok(ScopeGuard::into_inner(thread_context))
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
