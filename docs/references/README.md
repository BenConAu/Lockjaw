# External reference materials

External documentation + reference-implementation source consulted
during the cacheable-DMA migration design (see
`docs/cacheable-dma-migration-plan.md`). Stored locally so the
migration audit trail does not depend on external availability.

## Files

| File | Source | Used for |
|------|--------|----------|
| `bcm2711-peripherals.pdf` | https://datasheets.raspberrypi.com/bcm2711/bcm2711-peripherals.pdf | BCM2711 ARM Peripherals datasheet. Lookup: emmc2 interrupt routing (chapter 6); confirmed emmc2 controller registers are NOT publicly documented by Broadcom (the controller is Arasan IP, NDA). |
| `uboot-sdhci.c` | https://raw.githubusercontent.com/u-boot/u-boot/master/drivers/mmc/sdhci.c | U-Boot SDHCI core driver. Used to confirm `sdhci_transfer_data` polls `DATA_END` and calls `dma_unmap_single` (→ `invalidate_dcache_range`) for cache-invalidate as the AXI drain mechanism. The `udelay(10)` in the polling loop is polling-cadence pacing, NOT the completion mechanism. |
| `uboot-iproc_sdhci.c` | https://raw.githubusercontent.com/u-boot/u-boot/master/drivers/mmc/iproc_sdhci.c | U-Boot iProc/Broadcom SDHCI variant. Confirmed: no BCM2711-specific completion-path quirk; the core driver's cache-invalidate pattern applies. |

## What is NOT in here

- SD Host Controller Simplified Specification v3.00 — gated by
  Cloudflare licence click-through at sdcard.org; archive.org
  mirrors returned 503/404. The relevant facts (TRANSFER_COMPLETE
  per-spec semantics, no normative AXI-drain guarantee) are
  documented in the migration plan based on partial fetches; no
  full PDF stored.
- Arasan eMMC IP block datasheet — NDA, not publicly available.
- Linux `drivers/mmc/host/sdhci.c` source — fetched via
  WebFetch for the `sdhci_data_irq` → `sdhci_finish_data` →
  `dma_sync_sg_for_cpu` path; not stored verbatim here because
  it's GPL-licensed and the project's own source is not
  GPL-licensed. The migration plan summarizes what the path does.

## Provenance

All files downloaded 2026-05-24 during the design pass for the
cacheable-DMA migration. The U-Boot files are at the upstream
master tip as of that date; re-fetching may produce different
content.
