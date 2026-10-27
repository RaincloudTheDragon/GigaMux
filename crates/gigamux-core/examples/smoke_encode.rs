//! CLI smoke: run one encode batch (args: input_dir output_dir).

use std::env;
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use gigamux_core::encode::{run_batch, EncodeEvent, EncodeOptions};
use gigamux_core::gpu::GpuChoice;

fn main() {
    let mut args = env::args().skip(1);
    let input = PathBuf::from(args.next().expect("input_dir"));
    let output = PathBuf::from(args.next().expect("output_dir"));
    let opts = EncodeOptions {
        input_dir: input,
        output_dir: output,
        gpu: GpuChoice::Any,
        preserve_obs_metadata: false,
        only_names: None,
    };
    run_batch(opts, Arc::new(AtomicBool::new(false)), |ev| match ev {
        EncodeEvent::Log(m) => println!("log: {m}"),
        EncodeEvent::FileStarted { name, .. } => println!("start: {name}"),
        EncodeEvent::BatchStarted { .. } | EncodeEvent::FileProgress { .. } => {}
        EncodeEvent::FileSkipped { name } => println!("skip: {name}"),
        EncodeEvent::FileDone { name } => println!("done: {name}"),
        EncodeEvent::FileFailed { name, error } => println!("fail: {name}: {error}"),
        EncodeEvent::Finished {
            ok,
            skipped,
            failed,
        } => println!("finished ok={ok} skipped={skipped} failed={failed}"),
    });
}
