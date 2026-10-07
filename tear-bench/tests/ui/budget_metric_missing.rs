use tear_bench::matrix::{Budget, Budgets, na};

const N: Budget = na("every metric but one");

fn main() {
    let _ = Budgets {
        echo_warm: N,
        echo_gap: N,
        echo_pause: N,
        echo_pause_hop: N,
        round_trip: N,
        round_trip_tail: N,
        key: N,
        throughput: N,
        attach: N,
        startup: N,
        new_session: N,
        restart: N,
        spikes: N,
        paste: N,
        observer: N,
        resize: N,
        memory: N,
        keyframe: N,
        rpcs: N,
        wire_bytes: N,
        frames: N,
        parses: N,
        encodes: N,
        allocations: N,
        idle_ticks: N,
        idle_wakeups: N,
        paints: N,
        loss: N,
        answers: N,
        replays: N,
        modes: N,
    };
}
