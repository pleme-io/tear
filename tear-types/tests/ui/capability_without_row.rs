tear_types::capabilities! {
    SpawnArgs { wire: "spawn-args", advertised: true },
    PaneYurai { wire: "pane-yurai", advertised: true },
    Freio { wire: "freio", advertised: true },
}

fn main() {
    let _ = (CAPABILITY_ROWS, capability_row);
}
