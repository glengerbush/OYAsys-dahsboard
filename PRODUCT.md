# Product

<!-- impeccable:product-schema 1 -->

## Platform

web

## Stack

Native Rust monitor, embedded SQLite, static HTML/CSS/JavaScript and locally bundled charts. The browser interface is served by the same executable behind an existing reverse proxy.

## Users

Linux server operators checking temperatures, cooling-fan behavior, and resource trends over trusted networks.

## Product Purpose

Observe the entire server, with a dedicated P100 temperature/cooling-fan view, while minimizing measured CPU and RAM consumption. Monitoring continues without a browser and independently of other hosted applications.

## Capabilities and Constraints

- Available GPU, CPU, memory, temperature, fan, storage, network, and system telemetry.
- Dashboard-editable sampling and retention, default 10 seconds and 30 days; delete expired readings with no permanent summaries.
- Existing cooling controls remain untouched. Verify the physical cooling-fan mapping before assigning its readings.
- No alerts, threshold warnings, application login, public access, additional proxy, or hardware control.
- Manual initial installation and hardware verification on the monitored server.

## Brand Commitments

OYAsys. Sleek Grafana-style operational dashboard. Dense, legible panels and useful charts take priority over decoration.

## Evidence on Hand

Local resource measurements and their limitations are documented in [performance](docs/performance.md). Sensor availability, physical fan identity, and NVIDIA driver overhead require verification on the monitored server. Synthetic data is always labeled.

## Product Principles

- Spend resources on readings, not infrastructure.
- Missing data is never zero or fabricated continuity.
- Preserve existing cooling and routing behavior.
- Measure overhead; an arbitrary budget is not an optimization finish line.
