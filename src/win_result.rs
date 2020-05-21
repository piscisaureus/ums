use std::fmt::Debug;
use std::io;

use winapi::shared::minwindef::BOOL;
use winapi::shared::minwindef::DWORD;
use winapi::shared::minwindef::FALSE;
use winapi::shared::minwindef::TRUE;
use winapi::shared::ntdef::HANDLE;
use winapi::shared::ntdef::NULL;
use winapi::um::errhandlingapi::GetLastError;
use winapi::um::handleapi::INVALID_HANDLE_VALUE;

pub trait WinResult: Sized {
  type Ok;
  fn ok(self) -> Option<Self::Ok>;

  fn result(self) -> Result<Self::Ok, io::Error> {
    self.ok().ok_or_else(io::Error::get_last_error)
  }
  fn unwrap(self) -> Self::Ok {
    self.result().unwrap()
  }
  fn unwrap_err<Err>(self) -> Err
  where
    Self::Ok: Debug,
    Err: WinError,
  {
    self.ok().ok_or_else(Err::get_last_error).unwrap_err()
  }
}

impl WinResult for BOOL {
  type Ok = ();
  fn ok(self) -> Option<()> {
    match self {
      FALSE => None,
      TRUE => Some(()),
      _ => unreachable!(),
    }
  }
}

impl WinResult for HANDLE {
  type Ok = Self;
  fn ok(self) -> Option<Self> {
    match self {
      NULL | INVALID_HANDLE_VALUE => None,
      handle => Some(handle),
    }
  }
}

pub trait WinError: Debug {
  fn get_last_error() -> Self;
}

impl WinError for () {
  fn get_last_error() -> Self {}
}

impl WinError for DWORD {
  fn get_last_error() -> Self {
    unsafe { GetLastError() }
  }
}

impl WinError for io::Error {
  fn get_last_error() -> Self {
    Self::last_os_error()
  }
}
