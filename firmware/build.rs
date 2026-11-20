fn main() {
    for v in ["WIFI_SSID", "WIFI_PASS", "WORKER_URL", "WORKER_TOKEN"] {
        println!("cargo:rerun-if-env-changed={v}");
    }
    embuild::espidf::sysenv::output();
}
