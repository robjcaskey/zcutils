fn main() -> std::io::Result<()> {
    zcutils::zcnblk_shm_target::cli(std::env::args().skip(1))
}
