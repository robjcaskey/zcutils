fn main() -> std::io::Result<()> {
    zcutils::ofi_pipe::cli(std::env::args().skip(1))
}
