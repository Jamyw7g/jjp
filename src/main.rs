use getopts::Options;
use jjp::JjpBuilder;
use reqwest::Proxy;

fn main() -> anyhow::Result<()> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;

    let mut opts = Options::new();
    opts.optflag("", "no-mmap", "Default using mmap, disable mmap.")
        .optopt("o", "output", "Output filename.", "FILE")
        .optopt("p", "parallel", "Parallel number.", "PARALLEL")
        .optopt(
            "x",
            "proxy",
            "Default using env Proxy, support HTTP, Socks5.",
            "PROXY",
        )
        .optflag("", "no-proxy", "Disable using proxy.")
        .optflag("h", "help", "Show this help.");

    let args = std::env::args().skip(1).collect::<Vec<_>>();
    let matches = opts.parse(args)?;
    if matches.opt_present("h") {
        show_usage(&opts);
        return Ok(());
    }
    let output = matches.opt_str("o");
    let no_mmap = matches.opt_present("no-mmap");
    let proxy = matches.opt_str("x");
    let no_proxy = matches.opt_present("no-proxy");
    let parallel = matches.opt_str("p");

    let url = if !matches.free.is_empty() {
        matches.free[0].clone()
    } else {
        show_usage(&opts);
        return Ok(());
    };
    rt.block_on(async move {
        let mut jpb = JjpBuilder::new(url)?;
        if let Some(proxy) = proxy {
            let proxy = Proxy::all(proxy)?;
            jpb = jpb.proxy(proxy);
        }
        if let Some(pa) = parallel {
            let num = pa.parse()?;
            jpb = jpb.parallel_num(num);
        }
        if let Some(name) = output {
            jpb = jpb.name(name);
        }
        jpb = jpb.no_proxy(no_proxy);
        jpb = jpb.mmap(!no_mmap);

        let mut jjp = jpb.build().await?;
        jjp.run().await?;

        Ok(())
    })
}

fn show_usage(opt: &Options) {
    let brief = format!("Usage: {} [options...] <url>", env!("CARGO_PKG_NAME"));
    print!("{}", opt.usage(&brief));
}
