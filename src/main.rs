use getopts::Options;

use jjp::JjPBuilder;

fn main() -> anyhow::Result<()> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;

    let mut opts = Options::new();
    opts.optopt("o", "output", "Output filename.", "FILE")
        .optopt("p", "parallel", "Parallel number.", "PARALLEL")
        .optflag("h", "help", "Show this help.");

    let args = std::env::args().skip(1).collect::<Vec<_>>();
    let matches = opts.parse(args)?;
    if matches.opt_present("h") {
        show_usage(&opts);
        return Ok(());
    }
    let output = matches.opt_str("o");
    let parallel = matches.opt_str("p");

    let url = if !matches.free.is_empty() {
        matches.free[0].clone()
    } else {
        show_usage(&opts);
        return Ok(());
    };
    rt.block_on(async move {
        let mut jjp_builder = JjPBuilder::new(url);
        if let Some(output) = output {
            jjp_builder = jjp_builder.set_filename(output);
        }
        if let Some(parallel) = parallel {
            if let Ok(parallel) = parallel.parse() {
                jjp_builder = jjp_builder.set_parallel_num(parallel);    
            }
        }
        let jjp = jjp_builder.build()?;
        jjp.parallel_download().await?;
        Ok(())
    })
}

fn show_usage(opt: &Options) {
    let brief = format!("Usage: {} [options...] <url>", env!("CARGO_PKG_NAME"));
    print!("{}", opt.usage(&brief));
}
