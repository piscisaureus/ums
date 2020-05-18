use scopeguard::guard;
use std::io;
use std::iter::repeat;
use std::mem::align_of;
use std::mem::size_of_val;
use std::ptr::null_mut;
use winapi::shared::basetsd::*;
use winapi::shared::minwindef::*;
use winapi::shared::minwindef::{FALSE, TRUE};
use winapi::shared::ntdef::HANDLE;
use winapi::um::processthreadsapi::*;
use winapi::um::winbase::*;
use winapi::um::winnt::*;

const UMS_VERSION: DWORD = RTL_UMS_VERSION;
const PROC_THREAD_ATTRIBUTE_UMS_THREAD: DWORD_PTR =
  6 | 0x0001_0000 | 0x0002_0000;

fn main() {
  let ums_completion_list = create_ums_completion_list().unwrap();
  let _ = create_ums_thread(ums_completion_list);
}

unsafe extern "system" fn thread_main(_param: LPVOID) -> DWORD {
  123
}

fn create_ums_completion_list() -> io::Result<PUMS_COMPLETION_LIST> {
  unsafe {
    let mut ums_completion_list = null_mut();
    match CreateUmsCompletionList(&mut ums_completion_list) {
      TRUE => Ok(ums_completion_list),
      FALSE => Err(io::Error::last_os_error()),
      _ => unreachable!(),
    }
  }
}

fn create_ums_thread(
  ums_completion_list: PUMS_COMPLETION_LIST,
) -> io::Result<HANDLE> {
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
      Err(io::Error::last_os_error())
    } else {
      Ok(thread_handle)
    }
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
