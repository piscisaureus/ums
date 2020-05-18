use scopeguard::guard;
use std::io;
use std::iter::repeat;
use std::mem::align_of;
use std::mem::size_of_val;
use std::ptr::null_mut;
use winapi::shared::basetsd::*;
use winapi::shared::minwindef::*;
use winapi::shared::minwindef::{FALSE, TRUE};

use winapi::shared::winerror::*;
use winapi::um::errhandlingapi::*;
use winapi::um::handleapi::CloseHandle;
use winapi::um::processthreadsapi::*;
use winapi::um::winbase::*;
use winapi::um::winnt::*;

#[allow(non_camel_case_types)]
type UMS_SCHEDULER_REASON = RTL_UMS_SCHEDULER_REASON;
const UMS_VERSION: DWORD = RTL_UMS_VERSION;

const PROC_THREAD_ATTRIBUTE_UMS_THREAD: DWORD_PTR =
  6 | 0x0001_0000 | 0x0002_0000;

fn main() {
  //let ums_completion_list = create_ums_completion_list().unwrap();
  //let _ = create_ums_thread(ums_completion_list);
  run_ums_scheduler().unwrap()
}

struct UmsSchedulerState {
  pub completion_list: PUMS_COMPLETION_LIST,
}

unsafe impl Send for UmsSchedulerState {}
unsafe impl Sync for UmsSchedulerState {}

impl UmsSchedulerState {
  pub fn new(completion_list: PUMS_COMPLETION_LIST) -> Self {
    Self { completion_list }
  }

  pub fn as_void_ptr(&mut self) -> *mut VOID {
    cast_mut_void(self)
  }

  pub unsafe fn from_void_ptr<'a>(p: *mut VOID) -> &'a mut Self {
    &mut *cast_aligned(p)
  }
}

unsafe extern "system" fn ums_scheduler(
  reason: UMS_SCHEDULER_REASON,
  activation_payload: ULONG_PTR,
  scheduler_param: *mut VOID,
) {
  return;
  let ums_thread_context =
    ums_scheduler_impl(reason, activation_payload, scheduler_param).unwrap();
  loop {
    let ok = ExecuteUmsThread(ums_thread_context);
    assert!(ok == FALSE);
    let error = GetLastError();
    if error != ERROR_RETRY {
      Err(io::Error::from_raw_os_error(error as i32)).unwrap()
    }
  }
}

fn ums_scheduler_impl(
  reason: UMS_SCHEDULER_REASON,
  _activation_payload: ULONG_PTR,
  scheduler_param: *mut VOID,
) -> io::Result<PUMS_CONTEXT> {
  // On startup, store the UmsSchedulerState reference that we received in
  // `scheduler_param`. On subsequent invocations, read it out of this slot,
  let state = unsafe {
    static mut STATE: Option<&'static mut UmsSchedulerState> = None;
    if reason == UmsSchedulerStartup {
      let prev =
        STATE.replace(UmsSchedulerState::from_void_ptr(scheduler_param));
      assert!(prev.is_none());
    }
    STATE.as_mut().unwrap()
  };

  let thread_context = create_ums_thread(state.completion_list)?;
  Ok(thread_context)
}

fn run_ums_scheduler() -> io::Result<()> {
  unsafe {
    let mut completion_list = null_mut();
    let ok = CreateUmsCompletionList(&mut completion_list);
    bool_to_result(ok)?;
    assert!(!completion_list.is_null());
    let _completion_list_guard = guard(completion_list, |p| {
      let ok = DeleteUmsCompletionList(p);
      assert_eq!(ok, TRUE);
    });

    let mut scheduler_state = UmsSchedulerState::new(completion_list);
    let mut scheduler_startup_info = UMS_SCHEDULER_STARTUP_INFO {
      UmsVersion: UMS_VERSION,
      CompletionList: completion_list,
      SchedulerProc: Some(ums_scheduler),
      SchedulerParam: scheduler_state.as_void_ptr(),
    };
    let ok = EnterUmsSchedulingMode(&mut scheduler_startup_info);
    bool_to_result(ok)?;

    Ok(())
  }
}

fn bool_to_result(ok: BOOL) -> io::Result<()> {
  match ok {
    TRUE => Ok(()),
    FALSE => Err(io::Error::last_os_error())?,
    _ => unreachable!(),
  }
}

unsafe extern "system" fn thread_main(_param: LPVOID) -> DWORD {
  eprintln!("In da thread!!");
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

    let thread_handle = CreateRemoteThreadEx(
      GetCurrentProcess(),
      null_mut(),
      0,
      Some(thread_main),
      null_mut(),
      0,
      attr_list_ptr,
      null_mut(),
    );
    if thread_handle.is_null() {
      Err(io::Error::last_os_error())?;
    }

    let ok = CloseHandle(thread_handle);
    bool_to_result(ok)?;

    Ok(ums_context)
  }
}

fn cast_aligned<T, U>(p: *mut T) -> *mut U {
  let address = p as usize;
  let mask = align_of::<U>() - 1;
  assert_eq!(address & mask, 0);
  address as *mut U
}

fn cast_mut_void<T>(p: *const T) -> *mut VOID {
  p as *mut T as *mut VOID
}
