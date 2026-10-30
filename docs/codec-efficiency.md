# Comparing NVENC AV1 and HEVC for GigaMux

GigaMux defaults to CQ0 / CRF0 with an automatic maxrate derived from the source. That preserves visual quality well, but codec choice still changes size and speed. This note summarizes a Maxrate ladder on NVENC AV1 versus uncapped NVENC HEVC for a typical heavy OBS recording.

## The Issue

GigaMux's default target (CQ0 constrained against a maxrate automatically determined from the average bitrate of the source) will always† be smaller than a CBR source. However, NVENC's CQ is not cross-codec calibrated; nvAV1 at CQ0 spends more bitrate by default than nvHEVC (but not nvAVC), but it beats both nvHEVC and nvAVC on VMAF score and speed. So while nvAV1 will always have better quality than nvHEVC with the same maxrate (auto/matching override), it will also be a fair bit larger. This is because CQ0 isn’t the same operating point on AV1 and HEVC; while it's the same value in the encode settings, it lands differently on each codec's curve, and each codec's rate/quality curves differ anyway.

Thus, if GigaMux is to recommend a default codec, the documentation needs to be clear about the size discrepancy, so that users can choose to override AV1's CQ to a value that's roughly the same operating point as HEVC on auto, and beats it for quality and size.

†True for the intended OBS/CBR case when the ceiling is source-based and VBR can go under it; atypical sources may not *always* see a smaller result.

## Test setup

- Source: one gameplay Replay from an HEVC ~40 Mbps CBR OBS recording (same class of settings as the encoder screenshot in the README).
- Encodes: GigaMux hardware path on an RTX 4080 Super (NVENC) — nvHEVC at default auto maxrate, and nvAV1 at CQ0 with Maxrate overrides from 6–26 Mbps plus AV1 auto.
- Scores: libvmaf pooled mean, `n_threads` from the host, `n_subsample=4`.

These numbers are for this clip and GPU. Treat them as guidance, not a guarantee; run a short encode on your own footage and hardware before marrying a workflow.

HEVC auto baseline: VMAF 97.26, 349.5 MB, ~20 Mbps average.

## Charts
![AV1-VMAF-ladder.png](AV1-VMAF-ladder.png)
![AV1-size-ladder.png](AV1-size-ladder.png)
*Size wins for AV1 hold through ~24 Mbps. Leanest win on both VMAF and size is ~18 Mbps. At 26 Mbps and at AV1 auto, quality stays higher but the file is larger than HEVC auto.*

## Score table

| Cap | Mbps | VMAF | Δ vs HEVC auto | MB | vs HEVC size | Beats both? |
| --- | ---: | ---: | ---: | ---: | ---: | --- |
| HEVC auto | 19.97 | 97.2628 | — | 349.5 | — | baseline |
| AV1 6 Mbps | 6.06 | 96.7641 | −0.4987 | 106.0 | −69.7% | no |
| AV1 8 Mbps | 7.83 | 96.9723 | −0.2905 | 137.1 | −60.8% | no |
| AV1 10 Mbps | 9.57 | 97.0903 | −0.1725 | 167.5 | −52.1% | no |
| AV1 12 Mbps | 11.23 | 97.1625 | −0.1003 | 196.5 | −43.8% | no |
| AV1 15 Mbps | 13.66 | 97.2307 | −0.0321 | 239.2 | −31.6% | no |
| AV1 16 Mbps | 14.44 | 97.2473 | −0.0155 | 252.7 | −27.7% | no |
| AV1 18 Mbps | 15.89 | 97.2762 | +0.0134 | 278.2 | −20.4% | yes |
| AV1 20 Mbps | 17.25 | 97.2992 | +0.0364 | 301.9 | −13.6% | yes |
| AV1 22 Mbps | 18.53 | 97.3180 | +0.0552 | 324.3 | −7.2% | yes |
| AV1 24 Mbps | 19.88 | 97.3324 | +0.0696 | 347.9 | −0.5% | yes |
| AV1 26 Mbps | 20.98 | 97.3434 | +0.0806 | 367.2 | +5.1% | no |
| AV1 auto | 23.19 | 97.3606 | +0.0978 | 405.8 | +16.1% | no |

“Beats both” means VMAF ≥ HEVC auto and smaller than HEVC auto.

## Takeaways

1. Default AV1 is a bit size-costly. At GigaMux’s CQ0 + auto maxrate, nvAV1 scored highest (~97.36) but was ~16% larger than nvHEVC auto. It is coding more detail near the quality ceiling, not “free” compression.
2. A Maxrate cap can beat HEVC on both axes. On this clip, ~18 Mbps was the leanest setting that beat HEVC auto VMAF while staying ~20% smaller. Wins continued through ~24 Mbps; ~26 Mbps crossed above HEVC auto size.
3. Speed favors nvAV1. In the same NVENC tests, AV1 encodes finished much faster than HEVC at comparable quality targets—useful when wall-clock time matters as much as bytes.
4. Your mileage will vary with resolution, motion, OBS rate control, and GPU generation. Use GigaMux’s Maxrate override and a short VMAF check when tuning.

If you mainly want CQ0 without babysitting a cap, HEVC remains a strong size pick on NVENC (and the path for pre-Ada GPUs without AV1 NVENC). If you want the AV1 quality ceiling and can set a Maxrate, AV1 can be both smaller and higher-scoring than uncapped HEVC.
