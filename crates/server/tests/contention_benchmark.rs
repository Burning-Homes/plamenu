//! Exercise the benchmark driver with controlled HTTP failures, without seeding
//! or running the performance workload.

#[path = "../benches/contention/main.rs"]
#[allow(dead_code, reason = "tests exercise the mixed scenario only")]
mod contention;
