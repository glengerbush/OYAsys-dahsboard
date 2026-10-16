---
name: OYAsys monitoring console
description: Dense, restrained hardware monitoring with a Grafana-inspired hierarchy.
colors:
  primary: '#6fc8e6'
  fan: '#eeb76b'
  secondary: '#74a9ef'
  supporting-green: '#8accad'
  supporting-purple: '#b5a2eb'
  neutral-bg: '#101318'
  neutral-panel: '#181d25'
  neutral-border: '#2b3442'
  neutral-text: '#e8edf4'
  neutral-muted: '#a8b3c2'
typography:
  body:
    fontFamily: '-apple-system, BlinkMacSystemFont, Segoe UI, sans-serif'
    fontSize: '13px'
    lineHeight: 1.5
  headline:
    fontSize: '22px'
    fontWeight: 600
    lineHeight: 1.3
  panel-title:
    fontSize: '12px'
    fontWeight: 600
rounded:
  sm: '3px'
spacing:
  panel-gap: '12px'
  desktop-gutter: '28px'
  phone-gutter: '12px'
components:
  panel:
    backgroundColor: '{colors.neutral-panel}'
    textColor: '{colors.neutral-text}'
    rounded: '{rounded.sm}'
  button-primary:
    backgroundColor: '#32678d'
    textColor: '#ffffff'
    rounded: '{rounded.sm}'
    padding: '6px 12px'
---

## Overview

**Creative North Star: "Quiet operational clarity"**

A compact Grafana-inspired console: useful numbers and aligned time series lead. Precision, legibility, and source honesty carry the identity. No imagery, remote fonts, animated backgrounds, or decorative gauges.

**Key Characteristics:**

- Charcoal surfaces and subtle panel boundaries.
- Prominent P100 temperature and verified cooling-fan comparison.
- Compact controls, tabular numerals, explicit units.
- Clear availability and synthetic-data labels.

## Colors

Cyan anchors focus and primary measurements. Amber identifies fan RPM; supporting series use a restrained, distinguishable palette. Near-white text and muted secondary labels sit above charcoal surfaces.

**The Measurement Rule.** Color identifies a series or interaction, never a temperature threshold or alert state.

## Typography

Use the operating system's UI font stack and tabular numerals. Keep headings compact; metadata stays subordinate. Current values remain readable outside charts, with units beside values or in panel headings.

## Layout

A compact toolbar owns global range, refresh, and Settings. The P100 view spans the content width; temperature and cooling-fan plots stack on a shared time scale. Supporting panels use two columns, three on wide displays, and one on phones. Settings opens as a narrow right drawer. Hardware specifications and source availability follow the plots.

**The Shared Time Rule.** Related plots use the same range and synchronized cursor; different units stay on separate labeled axes.

## Elevation & Depth

Flat panels use quiet one-pixel boundaries. A modest drawer shadow distinguishes temporary settings from persistent content. No glass, glow, gradients, or decorative shadows.

## Shapes

Small, consistent corner rounding. Square chart geometry and restrained spacing communicate an operational tool, not oversized marketing tiles.

## Components

Buttons and inputs use native semantics, visible keyboard focus, and explicit labels. Save/Cancel and inline validation govern settings; shortening retention has a dedicated deletion acknowledgement. Canvas plots have current readouts and expandable accessible summary tables. Source details expose full identities without cluttering the main view.

**The Truthful State Rule.** Zero, unavailable, stale, disconnected, and synthetic states remain distinct. Unverified fan identity is never silently assigned.

## Do's and Don'ts

- Do preserve a shared time axis, readable units, and useful chart space.
- Do pause fetches and rendering in hidden tabs.
- Do keep controls usable on a narrow phone screen.
- Don't introduce external fonts, decorative gauges, or continuous animation.
- Don't present synthetic readings as live hardware.
- Don't add temperature warning colors or cooling controls.
