# Deployment work

For deployment, publication and release tasks, read and follow
[the deployment plan](docs/deployment-plan.md). Record verification and recovery
evidence for the exact revision and target being released. Update the plan
when implementation or operational behavior changes.

Quant is one Leptoskit application rooted at `leptoskit.toml`, with one Cargo
workspace and lockfile. Select destinations with `--app-target`; keep unavailable
targets marked until their implementation and release gates are satisfied.
