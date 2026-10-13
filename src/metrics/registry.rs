//! A small, dependency-free metrics registry with Prometheus text exposition.
//!
//! Supports counters, gauges and histograms, each optionally partitioned by a
//! fixed set of labels. All hot-path operations are lock-free atomics; label
//! lookup takes a read lock on a small map and only writes on first use.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use parking_lot::{Mutex, RwLock};

/// Monotonically increasing counter.
#[derive(Debug, Default)]
pub struct Counter(AtomicU64);

impl Counter {
    pub fn inc(&self) {
        self.inc_by(1);
    }

    pub fn inc_by(&self, n: u64) {
        self.0.fetch_add(n, Ordering::Relaxed);
    }

    pub fn get(&self) -> u64 {
        self.0.load(Ordering::Relaxed)
    }
}

/// A value that can go up and down. Stored as `f64` bits.
#[derive(Debug, Default)]
pub struct Gauge(AtomicU64);

impl Gauge {
    pub fn set(&self, v: f64) {
        self.0.store(v.to_bits(), Ordering::Relaxed);
    }

    pub fn add(&self, delta: f64) {
        let mut current = self.0.load(Ordering::Relaxed);
        loop {
            let next = (f64::from_bits(current) + delta).to_bits();
            match self
                .0
                .compare_exchange_weak(current, next, Ordering::Relaxed, Ordering::Relaxed)
            {
                Ok(_) => return,
                Err(actual) => current = actual,
            }
        }
    }

    pub fn inc(&self) {
        self.add(1.0);
    }

    pub fn dec(&self) {
        self.add(-1.0);
    }

    pub fn get(&self) -> f64 {
        f64::from_bits(self.0.load(Ordering::Relaxed))
    }
}

/// Default latency buckets in seconds (100µs .. 10s).
pub const LATENCY_BUCKETS: &[f64] = &[
    0.0001, 0.0005, 0.001, 0.0025, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0,
];

/// Cumulative histogram with fixed upper bounds.
#[derive(Debug)]
pub struct Histogram {
    bounds: Vec<f64>,
    counts: Vec<AtomicU64>,
    sum: Gauge,
    count: AtomicU64,
}

impl Histogram {
    pub fn new(bounds: &[f64]) -> Self {
        let mut bounds = bounds.to_vec();
        bounds.retain(|b| b.is_finite());
        bounds.sort_by(|a, b| a.total_cmp(b));
        bounds.dedup();
        let counts = bounds.iter().map(|_| AtomicU64::new(0)).collect();
        Histogram {
            bounds,
            counts,
            sum: Gauge::default(),
            count: AtomicU64::new(0),
        }
    }

    pub fn observe(&self, v: f64) {
        if let Some(i) = self.bounds.iter().position(|b| v <= *b) {
            self.counts[i].fetch_add(1, Ordering::Relaxed);
        }
        self.sum.add(v);
        self.count.fetch_add(1, Ordering::Relaxed);
    }

    pub fn count(&self) -> u64 {
        self.count.load(Ordering::Relaxed)
    }

    pub fn sum(&self) -> f64 {
        self.sum.get()
    }

    /// Cumulative counts per bucket bound, followed by `+Inf`.
    pub fn cumulative(&self) -> Vec<(f64, u64)> {
        let mut acc = 0;
        let mut out: Vec<(f64, u64)> = self
            .bounds
            .iter()
            .zip(&self.counts)
            .map(|(b, c)| {
                acc += c.load(Ordering::Relaxed);
                (*b, acc)
            })
            .collect();
        out.push((f64::INFINITY, self.count()));
        out
    }
}

/// Kind of a metric family, as printed in `# TYPE`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MetricKind {
    Counter,
    Gauge,
    Histogram,
}

impl MetricKind {
    fn as_str(self) -> &'static str {
        match self {
            MetricKind::Counter => "counter",
            MetricKind::Gauge => "gauge",
            MetricKind::Histogram => "histogram",
        }
    }
}

/// Something that renders itself in Prometheus text format.
pub trait Collect: Send + Sync {
    fn name(&self) -> &str;
    fn render(&self, out: &mut String);
}

/// Values that know how to print themselves as samples.
pub trait Sample: Send + Sync {
    const KIND: MetricKind;
    fn write_samples(&self, out: &mut String, name: &str, labels: &str);
}

impl Sample for Counter {
    const KIND: MetricKind = MetricKind::Counter;
    fn write_samples(&self, out: &mut String, name: &str, labels: &str) {
        let _ = writeln!(out, "{name}{} {}", wrap(labels), self.get());
    }
}

impl Sample for Gauge {
    const KIND: MetricKind = MetricKind::Gauge;
    fn write_samples(&self, out: &mut String, name: &str, labels: &str) {
        let _ = writeln!(out, "{name}{} {}", wrap(labels), format_float(self.get()));
    }
}

impl Sample for Histogram {
    const KIND: MetricKind = MetricKind::Histogram;
    fn write_samples(&self, out: &mut String, name: &str, labels: &str) {
        for (bound, count) in self.cumulative() {
            let le = if bound.is_infinite() {
                "+Inf".to_string()
            } else {
                format_float(bound)
            };
            let all = join_labels(labels, &format!("le=\"{le}\""));
            let _ = writeln!(out, "{name}_bucket{{{all}}} {count}");
        }
        let _ = writeln!(
            out,
            "{name}_sum{} {}",
            wrap(labels),
            format_float(self.sum())
        );
        let _ = writeln!(out, "{name}_count{} {}", wrap(labels), self.count());
    }
}

fn wrap(labels: &str) -> String {
    if labels.is_empty() {
        String::new()
    } else {
        format!("{{{labels}}}")
    }
}

fn join_labels(a: &str, b: &str) -> String {
    match (a.is_empty(), b.is_empty()) {
        (true, _) => b.to_string(),
        (_, true) => a.to_string(),
        _ => format!("{a},{b}"),
    }
}

fn format_float(v: f64) -> String {
    if v.is_nan() {
        "NaN".into()
    } else if v.is_infinite() {
        if v > 0.0 {
            "+Inf".into()
        } else {
            "-Inf".into()
        }
    } else if v.fract() == 0.0 && v.abs() < 1e15 {
        format!("{}", v as i64)
    } else {
        format!("{v}")
    }
}

/// Escapes a label value per the exposition format.
pub fn escape_label_value(v: &str) -> String {
    let mut out = String::with_capacity(v.len());
    for c in v.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            c => out.push(c),
        }
    }
    out
}

/// A metric family partitioned by a fixed list of label names.
pub struct Family<M: Sample> {
    name: String,
    help: String,
    label_names: Vec<&'static str>,
    children: RwLock<BTreeMap<Vec<String>, Arc<M>>>,
    make: Box<dyn Fn() -> M + Send + Sync>,
}

impl<M: Sample> Family<M> {
    pub fn new(
        name: &str,
        help: &str,
        label_names: &[&'static str],
        make: impl Fn() -> M + Send + Sync + 'static,
    ) -> Self {
        Family {
            name: name.to_string(),
            help: help.to_string(),
            label_names: label_names.to_vec(),
            children: RwLock::new(BTreeMap::new()),
            make: Box::new(make),
        }
    }

    /// Returns the child for the given label values, creating it on first use.
    ///
    /// # Panics
    /// Panics if the number of values differs from the number of label names;
    /// that is a programming error, not a runtime condition.
    pub fn with(&self, values: &[&str]) -> Arc<M> {
        assert_eq!(
            values.len(),
            self.label_names.len(),
            "label cardinality mismatch for {}",
            self.name
        );
        let key: Vec<String> = values.iter().map(|v| v.to_string()).collect();
        if let Some(found) = self.children.read().get(&key) {
            return found.clone();
        }
        self.children
            .write()
            .entry(key)
            .or_insert_with(|| Arc::new((self.make)()))
            .clone()
    }

    pub fn len(&self) -> usize {
        self.children.read().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl<M: Sample> Collect for Family<M> {
    fn name(&self) -> &str {
        &self.name
    }

    fn render(&self, out: &mut String) {
        let _ = writeln!(out, "# HELP {} {}", self.name, self.help);
        let _ = writeln!(out, "# TYPE {} {}", self.name, M::KIND.as_str());
        for (values, metric) in self.children.read().iter() {
            let labels = self
                .label_names
                .iter()
                .zip(values)
                .map(|(n, v)| format!("{n}=\"{}\"", escape_label_value(v)))
                .collect::<Vec<_>>()
                .join(",");
            metric.write_samples(out, &self.name, &labels);
        }
    }
}

/// An unlabelled metric with name and help text.
pub struct Single<M: Sample> {
    name: String,
    help: String,
    metric: Arc<M>,
}

impl<M: Sample> Single<M> {
    pub fn new(name: &str, help: &str, metric: M) -> Self {
        Single {
            name: name.to_string(),
            help: help.to_string(),
            metric: Arc::new(metric),
        }
    }

    pub fn metric(&self) -> Arc<M> {
        self.metric.clone()
    }
}

impl<M: Sample> Collect for Single<M> {
    fn name(&self) -> &str {
        &self.name
    }

    fn render(&self, out: &mut String) {
        let _ = writeln!(out, "# HELP {} {}", self.name, self.help);
        let _ = writeln!(out, "# TYPE {} {}", self.name, M::KIND.as_str());
        self.metric.write_samples(out, &self.name, "");
    }
}

/// Holds metric families and renders them in registration order.
#[derive(Default)]
pub struct Registry {
    collectors: Mutex<Vec<Arc<dyn Collect>>>,
}

impl Registry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers a collector.
    ///
    /// # Panics
    /// Panics on duplicate metric names, which would produce invalid output.
    pub fn register<C: Collect + 'static>(&self, collector: C) -> Arc<C> {
        let arc = Arc::new(collector);
        let mut collectors = self.collectors.lock();
        assert!(
            collectors.iter().all(|c| c.name() != arc.name()),
            "duplicate metric {}",
            arc.name()
        );
        collectors.push(arc.clone());
        arc
    }

    pub fn counter(&self, name: &str, help: &str) -> Arc<Counter> {
        self.register(Single::new(name, help, Counter::default()))
            .metric()
    }

    pub fn gauge(&self, name: &str, help: &str) -> Arc<Gauge> {
        self.register(Single::new(name, help, Gauge::default()))
            .metric()
    }

    pub fn histogram(&self, name: &str, help: &str, buckets: &[f64]) -> Arc<Histogram> {
        self.register(Single::new(name, help, Histogram::new(buckets)))
            .metric()
    }

    pub fn counter_family(
        &self,
        name: &str,
        help: &str,
        labels: &[&'static str],
    ) -> Arc<Family<Counter>> {
        self.register(Family::new(name, help, labels, Counter::default))
    }

    pub fn histogram_family(
        &self,
        name: &str,
        help: &str,
        labels: &[&'static str],
        buckets: &'static [f64],
    ) -> Arc<Family<Histogram>> {
        self.register(Family::new(name, help, labels, move || {
            Histogram::new(buckets)
        }))
    }

    /// Renders all metrics in Prometheus text exposition format (0.0.4).
    pub fn render(&self) -> String {
        let mut out = String::new();
        for c in self.collectors.lock().iter() {
            c.render(&mut out);
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counter_and_gauge_basics() {
        let c = Counter::default();
        c.inc();
        c.inc_by(4);
        assert_eq!(c.get(), 5);
        let g = Gauge::default();
        g.set(10.0);
        g.inc();
        g.dec();
        g.add(-2.5);
        assert_eq!(g.get(), 7.5);
    }

    #[test]
    fn histogram_buckets_are_cumulative() {
        let h = Histogram::new(&[1.0, 5.0, 0.5, 5.0]);
        for v in [0.1, 0.7, 3.0, 9.0] {
            h.observe(v);
        }
        let cumulative = h.cumulative();
        assert_eq!(
            cumulative,
            vec![(0.5, 1), (1.0, 2), (5.0, 3), (f64::INFINITY, 4)]
        );
        assert!((h.sum() - 12.8).abs() < 1e-9);
    }

    #[test]
    fn renders_exposition_format() {
        let reg = Registry::new();
        let requests = reg.counter_family("http_requests_total", "Requests", &["method", "status"]);
        requests.with(&["GET", "200"]).inc_by(3);
        requests.with(&["POST", "4\"0\n9"]).inc();
        let inflight = reg.gauge("in_flight", "In flight");
        inflight.set(2.0);
        let lat = reg.histogram("latency_seconds", "Latency", &[0.1, 1.0]);
        lat.observe(0.05);
        let text = reg.render();
        assert!(text.contains("# TYPE http_requests_total counter"));
        assert!(text.contains("http_requests_total{method=\"GET\",status=\"200\"} 3"));
        assert!(text.contains("status=\"4\\\"0\\n9\""));
        assert!(text.contains("in_flight 2"));
        assert!(text.contains("latency_seconds_bucket{le=\"0.1\"} 1"));
        assert!(text.contains("latency_seconds_bucket{le=\"+Inf\"} 1"));
        assert!(text.contains("latency_seconds_count 1"));
        assert_eq!(requests.len(), 2);
    }

    #[test]
    fn labelled_histograms_merge_le_label() {
        let reg = Registry::new();
        let fam = reg.histogram_family("op_seconds", "Ops", &["op"], LATENCY_BUCKETS);
        fam.with(&["post"]).observe(0.002);
        let text = reg.render();
        assert!(text.contains("op_seconds_bucket{op=\"post\",le=\"0.0025\"} 1"));
        assert!(text.contains("op_seconds_sum{op=\"post\"} 0.002"));
    }

    #[test]
    #[should_panic(expected = "duplicate metric")]
    fn duplicate_names_panic() {
        let reg = Registry::new();
        reg.counter("x_total", "x");
        reg.counter("x_total", "x");
    }

    #[test]
    fn float_formatting() {
        assert_eq!(format_float(3.0), "3");
        assert_eq!(format_float(0.25), "0.25");
        assert_eq!(format_float(f64::INFINITY), "+Inf");
        assert_eq!(format_float(f64::NAN), "NaN");
    }

    #[test]
    fn concurrent_updates_are_not_lost() {
        let reg = Registry::new();
        let fam = reg.counter_family("c_total", "c", &["k"]);
        let gauge = reg.gauge("g", "g");
        std::thread::scope(|s| {
            for _ in 0..8 {
                s.spawn(|| {
                    for _ in 0..1000 {
                        fam.with(&["a"]).inc();
                        gauge.add(1.0);
                    }
                });
            }
        });
        assert_eq!(fam.with(&["a"]).get(), 8000);
        assert_eq!(gauge.get(), 8000.0);
    }
}
