// Quick-and-dirty C DSP, per PLAN.md: a one-pole envelope follower with
// Schmitt-trigger thresholding. Skuiz needs no glue layer for this — the
// struct is plain data and the functions are plain C, so Rust calls them
// through `extern "C"` directly (see lib.rs, build.rs).

#include "envelope.h"

void skuiz_env_init(skuiz_env *e, float sample_rate) {
    e->env = 0.0f;
    e->open = 0;
    // ~5 ms attack, ~50 ms release, expressed as one-pole coefficients.
    e->attack = 1.0f - expf(-1.0f / (0.005f * sample_rate));
    e->release = 1.0f - expf(-1.0f / (0.050f * sample_rate));
}

// Returns the frame index of the first threshold crossing, or -1 for none.
// `hysteresis` keeps a signal hovering at the threshold from machine-gunning
// note events: the gate opens at `threshold` but only closes at 75% of it.
int skuiz_env_scan(skuiz_env *e, const float *samples, int frames,
                   float threshold, int *out_closed) {
    int crossing = -1;
    *out_closed = 0;
    for (int i = 0; i < frames; i++) {
        float x = fabsf(samples[i]);
        float k = (x > e->env) ? e->attack : e->release;
        e->env += k * (x - e->env);

        if (!e->open && e->env >= threshold) {
            e->open = 1;
            if (crossing < 0) {
                crossing = i;
            }
        } else if (e->open && e->env < threshold * 0.75f) {
            e->open = 0;
            *out_closed = 1;
        }
    }
    return crossing;
}
