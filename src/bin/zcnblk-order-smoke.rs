fn main() -> std::io::Result<()> {
    zcutils::zcnblk_order_smoke_cli(std::env::args().skip(1))
}
