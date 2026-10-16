# OYAsys domain glossary

OYAsys monitors a Linux server's hardware condition and resource use.

## Language

**Monitored server**:
The Linux machine whose hardware condition and resource use OYAsys reports.
_Avoid_: App server (ambiguous with a hosted application)

**P100**:
The monitored server's NVIDIA Tesla P100 GPU, given its own dedicated dashboard section.

**P100 cooling fan**:
The fan cooling the P100, connected to a motherboard fan header. It is separate from the GPU's onboard telemetry.

**Dashboard**:
The OYAsys view of the monitored server's available hardware readings and their history.

**Sampling interval**:
The configured time between successive collections of server readings.
_Avoid_: Refresh rate (ambiguous with dashboard updates)

**Full-detail retention**:
The configured duration for keeping individual collected readings before they expire.
_Avoid_: History (ambiguous with longer-term summaries)
