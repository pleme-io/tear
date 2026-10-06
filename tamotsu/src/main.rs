fn main() {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    tamotsu::main_from_args(&argv)
}
