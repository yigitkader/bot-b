//location: /crates/app/src/monitor.rs
// Monitoring and metrics (moved from monitor crate)

pub fn init_prom(metrics_port: u16) {
    use metrics_exporter_prometheus::PrometheusBuilder;
    let builder = PrometheusBuilder::new().with_http_listener(([0, 0, 0, 0], metrics_port));
    builder
        .install()
        .expect("failed to install prometheus recorder");
    tracing::info!(port = metrics_port, "prometheus exporter running");
}
