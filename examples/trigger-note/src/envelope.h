#ifndef SKUIZ_ENVELOPE_H
#define SKUIZ_ENVELOPE_H

#include <math.h>

typedef struct {
    float env;
    float attack;
    float release;
    int open;
} skuiz_env;

void skuiz_env_init(skuiz_env *e, float sample_rate);
int skuiz_env_scan(skuiz_env *e, const float *samples, int frames,
                   float threshold, int *out_closed);
float skuiz_env_level(const skuiz_env *e);

#endif
