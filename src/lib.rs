// I think there's nothing wrong with this pattern, so silence clippy about it.
#![allow(clippy::try_err)]

use derive_deref::*;
use scopeguard::guard;
use scopeguard::ScopeGuard;
use std::cell::RefCell;
use std::cell::RefMut;
use std::collections::VecDeque;
use std::io;
use std::iter::repeat;
use std::mem::align_of;
use std::mem::size_of_val;
use std::ptr::null_mut;
use winapi::shared::basetsd::*;
use winapi::shared::minwindef::*;
use winapi::shared::minwindef::{FALSE, TRUE};
use winapi::shared::winerror::*;
use winapi::um::errhandlingapi::GetLastError;
use winapi::um::handleapi::*;
use winapi::um::processthreadsapi::*;
use winapi::um::synchapi::*;
use winapi::um::winbase::*;
use winapi::um::winnt::*;

#[allow(non_camel_case_types)]
type UMS_SCHEDULER_REASON = RTL_UMS_SCHEDULER_REASON;

const UMS_VERSION: DWORD = RTL_UMS_VERSION;
const PROC_THREAD_ATTRIBUTE_UMS_THREAD: DWORD_PTR = 0x0003_0006;
const STACK_SIZE_PARAM_IS_A_RESERVATION: DWORD = 0x0001_0000;

static STATE: StaticState = StaticState(RefCell::new(None));

#[derive(Deref, DerefMut)]
struct StaticState(RefCell<Option<UmsSchedulerState>>);

unsafe impl Send for StaticState {}
unsafe impl Sync for StaticState {}

struct UmsSchedulerState {
  pub completion_list: PUMS_COMPLETION_LIST,
  pub runnable_threads: VecDeque<PUMS_CONTEXT>,
  pub thread_count: usize,
}

unsafe impl Send for UmsSchedulerState {}
unsafe impl Sync for UmsSchedulerState {}

impl UmsSchedulerState {
  pub fn new(completion_list: PUMS_COMPLETION_LIST) -> Self {
    Self {
      completion_list,
      runnable_threads: VecDeque::new(),
      thread_count: 0,
    }
  }

  pub fn init(completion_list: PUMS_COMPLETION_LIST) {
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

#[derive(Debug, Eq, PartialEq)]
#[allow(dead_code)] // False positive.
enum UmsSchedulerCallCause {
  Startup {
    param: *mut VOID,
  },
  ThreadBlockedOnTrap,
  ThreadBlockedOnSyscall,
  ThreadYield {
    thread_context: PUMS_CONTEXT,
    param: *mut VOID,
  },
  ThreadBusy {
    thread_context: PUMS_CONTEXT,
  },
  ThreadSuspended {
    thread_context: PUMS_CONTEXT,
  },
  ThreadTerminated {
    thread_context: PUMS_CONTEXT,
  },
  ThreadExecuteError {
    thread_context: PUMS_CONTEXT,
    error: DWORD,
  },
}

impl UmsSchedulerCallCause {
  pub fn from_scheduler_args(
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
        Self::ThreadBlockedOnTrap
      }
      UmsSchedulerThreadBlocked if activation_payload == 1 => {
        Self::ThreadBlockedOnSyscall
      }
      UmsSchedulerThreadYield => Self::ThreadYield {
        thread_context: activation_payload as PUMS_CONTEXT,
        param: scheduler_param,
      },
      _ => unreachable!(),
    }
  }

  pub fn from_execute_failure(thread_context: PUMS_CONTEXT) -> Self {
    match unsafe { GetLastError() } {
      ERROR_RETRY => Self::ThreadBusy { thread_context },
      ERROR_SUCCESS => unreachable!(),
      _ if Self::get_thread_flag(thread_context, UmsThreadIsTerminated) => {
        Self::ThreadTerminated { thread_context }
      }
      _ if Self::get_thread_flag(thread_context, UmsThreadIsSuspended) => {
        Self::ThreadSuspended { thread_context }
      }
      error => Self::ThreadExecuteError {
        thread_context,
        error,
      },
    }
  }

  fn get_thread_flag(
    thread_context: PUMS_CONTEXT,
    thread_info_class: UMS_THREAD_INFO_CLASS,
  ) -> bool {
    let mut flag: BOOLEAN = 0;
    let mut return_length: ULONG = 0;
    let ok = unsafe {
      QueryUmsThreadInformation(
        thread_context,
        thread_info_class,
        &mut flag as *mut _ as *mut VOID,
        size_of_val(&flag) as ULONG,
        &mut return_length,
      )
    };
    match ok {
      TRUE => {
        assert_eq!(return_length as usize, size_of_val(&flag));
        flag != 0
      }
      FALSE => false,
      _ => unreachable!(),
    }
  }
}

extern "system" fn ums_scheduler(
  reason: UMS_SCHEDULER_REASON,
  activation_payload: ULONG_PTR,
  scheduler_param: *mut VOID,
) {
  // The `ExecuteUmsThread()` does not return if succesful, and certainly Rust
  // does not expect that, so we are at risk of skipping Drop handlers.
  // Therefore most of the "meat and potatoes" lives in `ums_scheduler_impl()`.
  // We just have to be sure that no variables in this function require cleanup.
  let mut cause = UmsSchedulerCallCause::from_scheduler_args(
    reason,
    activation_payload,
    scheduler_param,
  );
  while let Some(thread_context) = ums_scheduler_impl(cause) {
    let ok = unsafe { ExecuteUmsThread(thread_context) };
    // If `ExecuteUmsThread()` returns, it has failed.
    assert_eq!(ok, FALSE);
    cause = UmsSchedulerCallCause::from_execute_failure(thread_context);
  }
}

fn ums_scheduler_impl(cause: UmsSchedulerCallCause) -> Option<PUMS_CONTEXT> {
  eprintln!("S: {:?}", cause);
  let mut state = UmsSchedulerState::get();

  use UmsSchedulerCallCause::*;
  match cause {
    Startup { .. } => {
      for _ in 0..4 {
        let _ = create_ums_thread(state.completion_list).unwrap();
        state.thread_count += 1;
      }
    }
    ThreadTerminated { thread_context } => {
      let ok = unsafe { DeleteUmsThreadContext(thread_context) };
      bool_to_result(ok).unwrap();
      state.thread_count -= 1;
      if state.thread_count == 0 {
        return None;
      }
    }
    ThreadYield { thread_context, .. }
    | ThreadBusy { thread_context }
    | ThreadSuspended { thread_context } => {
      state.runnable_threads.push_back(thread_context);
    }
    ThreadExecuteError {
      thread_context,
      error,
    } => panic!("Failed to execute thread {:?}: {}", thread_context, error),
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
        let mut list_item: PUMS_CONTEXT = null_mut();
        let ok = unsafe {
          DequeueUmsCompletionListItems(
            state.completion_list,
            INFINITE,
            &mut list_item,
          )
        };
        bool_to_result(ok).unwrap();
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
    let mut completion_list: PUMS_COMPLETION_LIST = null_mut();
    let ok = CreateUmsCompletionList(&mut completion_list);
    bool_to_result(ok)?;
    assert!(!completion_list.is_null());
    let _completion_list_guard = guard(completion_list, |p| {
      let ok = DeleteUmsCompletionList(p);
      assert_eq!(ok, TRUE);
    });

    UmsSchedulerState::init(completion_list);
    assert_eq!(UmsSchedulerState::get().completion_list, completion_list);

    let mut scheduler_startup_info = UMS_SCHEDULER_STARTUP_INFO {
      UmsVersion: UMS_VERSION,
      CompletionList: completion_list,
      SchedulerProc: Some(ums_scheduler),
      SchedulerParam: null_mut(),
    };
    let ok = EnterUmsSchedulingMode(&mut scheduler_startup_info);
    bool_to_result(ok)?;

    Ok(())
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
  completion_list: PUMS_COMPLETION_LIST,
) -> io::Result<PUMS_CONTEXT> {
  unsafe {
    let mut thread_context = null_mut();
    let ok = CreateUmsThreadContext(&mut thread_context);
    bool_to_result(ok)?;
    let thread_context = guard(thread_context, |c| {
      let ok = DeleteUmsThreadContext(c);
      bool_to_result(ok).unwrap();
    });

    let mut attr_list_size = 0;
    let ok =
      InitializeProcThreadAttributeList(null_mut(), 1, 0, &mut attr_list_size);
    assert_eq!(ok, FALSE);
    assert_ne!(attr_list_size, 0);

    let mut attr_list = repeat(0u8).take(attr_list_size).collect::<Box<[u8]>>();
    let attr_list: *mut PROC_THREAD_ATTRIBUTE_LIST =
      cast_aligned(attr_list.as_mut_ptr());
    let ok =
      InitializeProcThreadAttributeList(attr_list, 1, 0, &mut attr_list_size);
    bool_to_result(ok)?;
    let attr_list = guard(attr_list, |p| DeleteProcThreadAttributeList(p));

    let attr = UMS_CREATE_THREAD_ATTRIBUTES {
      UmsVersion: UMS_VERSION,
      UmsContext: *thread_context,
      UmsCompletionList: completion_list,
    };

    let ok = UpdateProcThreadAttribute(
      *attr_list,
      0,
      PROC_THREAD_ATTRIBUTE_UMS_THREAD,
      cast_mut_void(&attr),
      size_of_val(&attr),
      null_mut(),
      null_mut(),
    );
    bool_to_result(ok)?;

    let thread_handle = CreateRemoteThreadEx(
      GetCurrentProcess(),
      null_mut(),
      1 << 20,
      Some(thread_main),
      null_mut(),
      STACK_SIZE_PARAM_IS_A_RESERVATION,
      *attr_list,
      null_mut(),
    );
    match thread_handle {
      h if h.is_null() => Err(io::Error::last_os_error())?,
      h => bool_to_result(CloseHandle(h))?,
    };

    eprintln!("UCTX: {:X?}", *thread_context);

    Ok(ScopeGuard::into_inner(thread_context))
  }
}

fn bool_to_result(ok: BOOL) -> io::Result<()> {
  match ok {
    TRUE => Ok(()),
    FALSE => Err(io::Error::last_os_error())?,
    _ => unreachable!(),
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
