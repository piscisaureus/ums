//! This module contains definitions that we need, but are not present in the
//! 'winapi' crate.

#![allow(non_camel_case_types)]

use winapi::ctypes::c_void;
use winapi::shared::basetsd::DWORD_PTR;
use winapi::shared::minwindef::DWORD;
use winapi::um::winnt::RTL_UMS_SCHEDULER_REASON;
use winapi::um::winnt::RTL_UMS_VERSION;

pub type UMS_CONTEXT = c_void;
pub type UMS_COMPLETION_LIST = c_void;
pub type UMS_SCHEDULER_REASON = RTL_UMS_SCHEDULER_REASON;

pub const UMS_VERSION: DWORD = RTL_UMS_VERSION;
pub const PROC_THREAD_ATTRIBUTE_UMS_THREAD: DWORD_PTR = 0x0003_0006;
pub const STACK_SIZE_PARAM_IS_A_RESERVATION: DWORD = 0x0001_0000;
