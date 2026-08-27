use std::env;
use std::io::{self, Read, Write};
use std::net::Shutdown;
use std::os::unix::net::UnixStream;

fn main() {
    if let Err(error) = run() {
        eprintln!("zcnblk-direct-migratectl: ERROR: {error}");
        std::process::exit(1);
    }
}

fn run() -> io::Result<()> {
    let mut args = env::args().skip(1);
    let socket = args.next().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "usage: zcnblk-direct-migratectl SOCKET COMMAND [ARG ...]",
        )
    })?;
    let fields = args.collect::<Vec<_>>();
    if fields.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "migration control command is empty",
        ));
    }
    let mut stream = UnixStream::connect(&socket)?;
    stream.write_all(fields.join(" ").as_bytes())?;
    stream.write_all(b"\n")?;
    stream.shutdown(Shutdown::Write)?;
    let mut response = String::new();
    stream.read_to_string(&mut response)?;
    print!("{response}");
    io::stdout().flush()?;
    if !response.starts_with("OK ") {
        return Err(io::Error::other(format!(
            "migration controller rejected the command: {}",
            response.trim_end()
        )));
    }
    Ok(())
}
