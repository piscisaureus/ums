#![allow(unused_imports)]

use std::cell::UnsafeCell;
use std::fs::read_dir;
use std::fs::ReadDir;
use std::iter::Iterator;
use std::mem::drop;
use std::ptr::NonNull;

use ums::*;
use winapi::um::handleapi::*;
use winapi::um::sysinfoapi::*;

static mut CTR: u64 = 0;

fn iter_subdirs(iter: ReadDir) -> impl Iterator<Item = ReadDir> {
  iter
    .filter_map(|e| e.ok())
    .filter(|e| {
      e.metadata()
        .map(|m| m.file_type())
        .map(|ft| ft.is_dir() && !ft.is_symlink())
        .unwrap_or(false)
    })
    .map(|e| e.path())
    .filter_map(|p| read_dir(&p).ok())
}

struct Stack<T>(NonNull<Vec<T>>);
impl<T> Default for Stack<T> {
  fn default() -> Self {
    Self(NonNull::new(Box::into_raw(Box::new(Vec::new()))).unwrap())
  }
}

impl<T> Copy for Stack<T> {}

impl<T> Clone for Stack<T> {
  fn clone(&self) -> Self {
    *self
  }
}

unsafe impl<T> Send for Stack<T> {}
unsafe impl<T> Sync for Stack<T> {}

impl<T> Stack<T> {
  pub unsafe fn get(&mut self) -> &mut Vec<T> {
    &mut *self.0.as_mut()
  }
}

fn main() {
  let dir_stack = Stack::default();
  loop {
    run_ums_scheduler(move || {
      for i in 0..10 {
        unsafe { CTR = 0 };
        let use_ums = i & 1 == 0;
        let mut d = unsafe { GetTickCount64() };
        for _ in 0..20_000 {
          unsafe { CTR += 1 };
          let f = move |mut dir_stack: Stack<_>| {
            let dir_stack = unsafe { dir_stack.get() };
            loop {
              match dir_stack.last_mut() {
                None => {
                  let root_iter = read_dir("r:\\").map(iter_subdirs).unwrap();
                  dir_stack.push(root_iter);
                  continue;
                }
                Some(dir_iter) => {
                  if let Some(rd) = dir_iter.next() {
                    let subdir_iter = iter_subdirs(rd);
                    dir_stack.push(subdir_iter);
                    return;
                  }
                }
              };
              dir_stack.pop();
            }
          };
          if use_ums {
            let dir_stack = dir_stack;
            blocking(move || f(dir_stack));
          } else {
            f(dir_stack);
          }
        }
        d = unsafe { GetTickCount64() } - d;
        let dd = d as f64 / 1000f64;
        let ctr = unsafe { CTR };
        let rate = (ctr as f64) / dd;
        println!(
          "UMS: {:5?} Switches: {}  Time: {:0.2}s  Rate: {}/s",
          use_ums,
          ctr,
          dd,
          rate.round()
        );
      }
    })
    .unwrap();
  }
}
