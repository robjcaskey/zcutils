fn main() -> std::io::Result<()> {
    zcutils::crypto_policy::initialize_or_exit();
    zcutils::zcnblk_order_smoke_cli(std::env::args().skip(1))
}
