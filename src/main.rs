use ums::*;
use winapi::um::handleapi::*;
use winapi::um::sysinfoapi::*;

static mut CTR: u64 = 0;

fn main() {
  run_ums_scheduler(|| {
    for _ in 0..10 {
      unsafe { CTR = 0 };
      let mut d = unsafe { GetTickCount64() };
      for _ in 0..100_000 {
        unsafe { CTR += 1 };
        #[cfg(feature = "ums")]
        blocking(|| {
          //std::fs::metadata("d:\\").unwrap();
          //std::env::var("Path").unwrap();
          unsafe {
            CloseHandle(INVALID_HANDLE_VALUE);
          }
        });
        #[cfg(not(feature = "ums"))]
        {
          //std::env::var("Path").unwrap();
          //std::fs::metadata("d:\\").unwrap();
          unsafe {
            CloseHandle(INVALID_HANDLE_VALUE);
          }
        }
      }
      d = unsafe { GetTickCount64() } - d;
      let dd = d as f64 / 1000f64;
      let ctr = unsafe { CTR };
      let rate = (ctr as f64) / dd;
      println!(
        "Switches: {}  Time: {:0.2}s  Rate: {}/s",
        ctr,
        dd,
        rate.round()
      );
    }
  })
  .unwrap();
}
