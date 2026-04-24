fn main() {
    // Propagates ESP-IDF build environment variables required by esp-idf-sys.
    embuild::espidf::sysenv::output();
}
