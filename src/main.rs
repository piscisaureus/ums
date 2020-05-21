// Disable warnings that I don't thing are helpful.
#![allow(clippy::try_err)]

use derive_deref::*;
use scopeguard::guard;

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
use winapi::um::errhandlingapi::GetLastError;
use winapi::um::handleapi::*;
use winapi::um::processthreadsapi::*;
use winapi::um::synchapi::*;
use winapi::um::winbase::*;
use winapi::um::winnt::*;

#[allow(non_camel_case_types)]
type UMS_SCHEDULER_REASON = RTL_UMS_SCHEDULER_REASON;
const UMS_VERSION: DWORD = RTL_UMS_VERSION;

const PROC_THREAD_ATTRIBUTE_UMS_THREAD: DWORD_PTR =
  6 | 0x0001_0000 | 0x0002_0000;

const STACK_SIZE_PARAM_IS_A_RESERVATION: DWORD = 0x0001_0000;

fn main() {
  unsafe { Sleep(1000) };
  run_ums_scheduler().unwrap()
}

static STATE: StaticState = StaticState(RefCell::new(None));

#[derive(Deref, DerefMut)]
struct StaticState(RefCell<Option<UmsSchedulerState>>);

unsafe impl Send for StaticState {}
unsafe impl Sync for StaticState {}

struct UmsSchedulerState {
  pub completion_list: PUMS_COMPLETION_LIST,
  pub main_thread: PUMS_CONTEXT,
  pub ready_threads: VecDeque<PUMS_CONTEXT>,
}

unsafe impl Send for UmsSchedulerState {}
unsafe impl Sync for UmsSchedulerState {}

impl UmsSchedulerState {
  pub fn new(completion_list: PUMS_COMPLETION_LIST) -> Self {
    Self {
      completion_list,
      main_thread: null_mut(),
      ready_threads: VecDeque::new(),
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

  pub fn from_last_error(thread_context: PUMS_CONTEXT) -> Self {
    let error = unsafe { GetLastError() };
    assert_ne!(error, 0);
    Self::ThreadExecuteError {
      thread_context,
      error,
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
    // ExecuteUmsThread does not return if successful.
    assert_eq!(ok, FALSE);
    cause = UmsSchedulerCallCause::from_last_error(thread_context);
  }
}

fn ums_scheduler_impl(cause: UmsSchedulerCallCause) -> Option<PUMS_CONTEXT> {
  eprintln!("S: {:?}", cause);
  let mut state = UmsSchedulerState::get();

  match cause {
    UmsSchedulerCallCause::Startup { .. } => {
      let _main_thread = create_ums_thread(state.completion_list).unwrap();
      let _main_thread = create_ums_thread(state.completion_list).unwrap();
      let main_thread = create_ums_thread(state.completion_list).unwrap();
      state.main_thread = main_thread;
    }
    UmsSchedulerCallCause::ThreadYield { thread_context, .. }
    | UmsSchedulerCallCause::ThreadExecuteError { thread_context, .. } => {
      state.ready_threads.push_back(thread_context);
    }
    _ => {}
  };

  loop {
    eprintln!("selecting. ready# = {}", state.ready_threads.len());
    match state.ready_threads.pop_front() {
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
          state.ready_threads.push_back(list_item);
          list_item = unsafe { GetNextUmsListItem(list_item) };
        }
      }
    }
  }
}

fn run_ums_scheduler() -> io::Result<()> {
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

unsafe extern "system" fn thread_main(_param: LPVOID) -> DWORD {
  for _ in 0..10 {
    println!("In da thread!!");
    Sleep(1000);
  }
  123
}

fn create_ums_thread(
  ums_completion_list: PUMS_COMPLETION_LIST,
) -> io::Result<PUMS_CONTEXT> {
  unsafe {
    let mut ums_context = null_mut();
    let ok = CreateUmsThreadContext(&mut ums_context);
    assert_eq!(ok, TRUE);

    let mut attr_list_size = 0;
    let ok =
      InitializeProcThreadAttributeList(null_mut(), 1, 0, &mut attr_list_size);
    assert_eq!(ok, FALSE);
    assert_ne!(attr_list_size, 0);

    let mut attr_list_box =
      repeat(0u8).take(attr_list_size).collect::<Box<[u8]>>();
    let attr_list_ptr: *mut PROC_THREAD_ATTRIBUTE_LIST =
      cast_aligned(attr_list_box.as_mut_ptr());

    let ok = InitializeProcThreadAttributeList(
      attr_list_ptr,
      1,
      0,
      &mut attr_list_size,
    );
    assert_eq!(ok, TRUE);
    let _attr_list_guard =
      guard(attr_list_ptr, |p| DeleteProcThreadAttributeList(p));

    let attr = UMS_CREATE_THREAD_ATTRIBUTES {
      UmsVersion: UMS_VERSION,
      UmsContext: ums_context,
      UmsCompletionList: ums_completion_list,
    };

    let ok = UpdateProcThreadAttribute(
      attr_list_ptr,
      0,
      PROC_THREAD_ATTRIBUTE_UMS_THREAD,
      cast_mut_void(&attr),
      size_of_val(&attr),
      null_mut(),
      null_mut(),
    );
    assert_eq!(ok, TRUE);

    let mut thread_id: DWORD = 0;
    let thread_handle = CreateRemoteThreadEx(
      GetCurrentProcess(),
      null_mut(),
      1 << 20,
      Some(thread_main),
      null_mut(),
      STACK_SIZE_PARAM_IS_A_RESERVATION,
      attr_list_ptr,
      &mut thread_id,
    );
    if thread_handle.is_null() {
      Err(io::Error::last_os_error())?;
    }

    eprintln!("tid: {}  UCTX: {:X?}", thread_id, ums_context);

    let ok = CloseHandle(thread_handle);
    bool_to_result(ok)?;

    Ok(ums_context)
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
