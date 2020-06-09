use ums::*;
#[allow(unused_imports)]
use winapi::um::handleapi::*;
use winapi::um::sysinfoapi::*;

static mut CTR: u64 = 0;

fn main() {
  loop {
    run_ums_scheduler(|| {
      for i in 0..10 {
        unsafe { CTR = 0 };
        let use_ums = i & 1 == 0;
        let mut d = unsafe { GetTickCount64() };
        for _ in 0..100_000 {
          unsafe { CTR += 1 };
          let f = || {
            //unsafe { CloseHandle(INVALID_HANDLE_VALUE) };
            std::fs::metadata("\\").unwrap();
            //std::env::var("Path").unwrap();
            //let _ = std::fs::File::open("d:\\ums\\stat.js");
            //std::process::Command::new("cmd.exe")
            //  .arg("/c")
            //  .arg("echo")
            //  .arg("Hoi!")
            //  .output()
            //  .unwrap();
          };
          if use_ums {
            blocking(f);
          } else {
            f();
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
