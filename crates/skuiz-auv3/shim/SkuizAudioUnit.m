//  SkuizAudioUnit.m — see SkuizAudioUnit.h.
//
//  Everything here is glue. The rules that shape it:
//
//  * Blocks used on the render thread must not capture `self`, or the unit
//    can be retained (or released) from the audio thread. They capture the
//    opaque Rust instance pointer instead.
//  * Nothing in `internalRenderBlock` may allocate, lock, or message an
//    Objective-C object beyond the render blocks the host handed us.
//  * The Rust side owns all state; this class holds no parameter values of
//    its own, so there is nothing here to keep in sync.

#import "SkuizAudioUnit.h"
#import <AVFoundation/AVFoundation.h>

/// Mirrors `SkuizParamInfo` in crates/skuiz-auv3/src/lib.rs.
typedef struct {
    uint32_t id;
    const char *name;
    double min;
    double max;
    double defaultValue;
    uint32_t choiceCount;
} SkuizParamInfo;

/// Mirrors `SkuizAudioBusInfo` in crates/skuiz-auv3/src/lib.rs.
typedef struct {
    uint32_t channelCount;
    uint8_t optional;
    const char *_Nullable name;
} SkuizAudioBusInfo;

/// Bounds of the bus topology, mirroring `MAX_BUS_CHANNELS` and
/// `MAX_BUSES_PER_DIRECTION` in crates/skuiz-core/src/bus.rs.
#define kSkuizMaxBusChannels 8u
#define kSkuizMaxBusesPerDirection 4u

/// Mirrors `SkuizAudioBusBuffers` in crates/skuiz-auv3/src/lib.rs: one bus's
/// channel pointers for a render call. The render entry point takes a fixed
/// array of `kSkuizMaxBusesPerDirection` of these per direction.
typedef struct {
    float *_Nullable channels[kSkuizMaxBusChannels];
    uint32_t channelCount;
    uint8_t active;
} SkuizAudioBusBuffers;

extern void *skuiz_auv3_init(const char *appGroupDir);
extern void skuiz_auv3_destroy(void *inst);
extern void skuiz_auv3_activate(void *inst, double sampleRate, uint32_t maxFrames);
extern void skuiz_auv3_deactivate(void *inst);
extern uint32_t skuiz_auv3_audio_bus_count(uint8_t direction);
extern bool skuiz_auv3_audio_bus_info(uint8_t direction, uint32_t index,
                                      SkuizAudioBusInfo *out);
extern void skuiz_auv3_render(void *inst, const SkuizAudioBusBuffers *inputs,
                              const SkuizAudioBusBuffers *outputs, uint32_t frames);
extern uint32_t skuiz_auv3_param_count(void);
extern bool skuiz_auv3_param_info(uint32_t index, SkuizParamInfo *out);
extern const char *skuiz_auv3_choice_label(uint32_t paramID, uint32_t choiceIndex);
extern double skuiz_auv3_get_param(void *inst, uint32_t id);
extern void skuiz_auv3_set_param(void *inst, uint32_t id, double value);
extern void skuiz_auv3_set_param_from_render(void *inst, uint32_t id, double value);
extern uint32_t skuiz_auv3_save_state(void *inst, uint8_t *buf, uint32_t cap);
extern bool skuiz_auv3_load_state(void *inst, const uint8_t *buf, uint32_t len);
extern void skuiz_auv3_reset(void *inst);
extern uint32_t skuiz_auv3_midi_count(void *inst);
extern bool skuiz_auv3_midi_event(void *inst, uint32_t index, uint32_t *frame, uint8_t *bytes3);

/// Key the state blob is stored under inside `fullState`.
static NSString *const kSkuizStateKey = @"skuiz.state";

/// Timed parameter events one block may carry before the excess is dropped.
/// Fixed-size and stack-allocated: the render thread must not allocate.
#define kSkuizMaxParamEvents 64u

typedef struct {
    uint32_t frame;
    uint32_t address;
    double value;
} SkuizParamEvent;

/// Preallocated pull targets for the sidechain input buses (every declared
/// input past the main one). The render block must not allocate, so these
/// are sized to `maximumFramesToRender` in `allocateRenderResources` and
/// freed in `deallocateRenderResources`. The main input needs no storage:
/// it is pulled straight into the host's output buffers.
typedef struct {
    /// Frames each channel buffer holds; blocks larger than this render
    /// with sidechains inactive rather than overrunning the buffers.
    uint32_t capacity;
    /// One buffer list per declared sidechain bus (slot 0 stays NULL). A
    /// host may substitute its own `mData` pointers when pulled, so the
    /// render block re-points these at `data` before every pull.
    AudioBufferList *_Nullable lists[kSkuizMaxBusesPerDirection];
    float *_Nullable data[kSkuizMaxBusesPerDirection];
} SkuizPullStorage;

/// Render the segment `[start, end)` of every connected bus and hand the
/// segment's MIDI to the host, offset back into block time. Called once per
/// automation segment, so MIDI frames the DSP stamps are segment-relative.
static void SkuizRenderSegment(void *instance, const SkuizAudioBusBuffers *inputs,
                               const SkuizAudioBusBuffers *outputs, uint32_t start, uint32_t end,
                               AUMIDIOutputEventBlock midiOut, const AudioTimeStamp *timestamp) {
    SkuizAudioBusBuffers segmentInputs[kSkuizMaxBusesPerDirection];
    SkuizAudioBusBuffers segmentOutputs[kSkuizMaxBusesPerDirection];
    memcpy(segmentInputs, inputs, sizeof(segmentInputs));
    memcpy(segmentOutputs, outputs, sizeof(segmentOutputs));
    for (uint32_t b = 0; b < kSkuizMaxBusesPerDirection; b++) {
        for (uint32_t c = 0; c < segmentInputs[b].channelCount; c++) {
            if (segmentInputs[b].channels[c] != NULL) {
                segmentInputs[b].channels[c] += start;
            }
        }
        for (uint32_t c = 0; c < segmentOutputs[b].channelCount; c++) {
            if (segmentOutputs[b].channels[c] != NULL) {
                segmentOutputs[b].channels[c] += start;
            }
        }
    }
    skuiz_auv3_render(instance, segmentInputs, segmentOutputs, end - start);

    if (midiOut != NULL) {
        uint32_t segmentFrames = end - start;
        uint32_t midiCount = skuiz_auv3_midi_count(instance);
        for (uint32_t i = 0; i < midiCount; i++) {
            uint32_t frame = 0;
            uint8_t bytes[3] = {0, 0, 0};
            if (skuiz_auv3_midi_event(instance, i, &frame, bytes)) {
                // The DSP is trusted for content, not for timing: an offset
                // past the end of the segment is clamped rather than handed
                // to the host.
                if (frame >= segmentFrames) {
                    frame = segmentFrames - 1;
                }
                midiOut((AUEventSampleTime)(timestamp->mSampleTime) +
                            (AUEventSampleTime)(start + frame),
                        0, sizeof(bytes), bytes);
            }
        }
    }
}

static NSString *_Nullable gSkuizAppGroupDirectory = nil;

/// One `AUAudioUnitBus` from the declared topology (`direction` 0 = input,
/// 1 = output), named after the bus and sized to its layout.
static AUAudioUnitBus *_Nullable SkuizCreateBus(uint8_t direction, uint32_t index,
                                                NSError **outError) {
    SkuizAudioBusInfo info;
    if (!skuiz_auv3_audio_bus_info(direction, index, &info) || info.channelCount == 0 ||
        info.channelCount > kSkuizMaxBusChannels) {
        return nil;
    }
    AVAudioFormat *format =
        [[AVAudioFormat alloc] initStandardFormatWithSampleRate:44100.0
                                                       channels:info.channelCount];
    AUAudioUnitBus *bus = [[AUAudioUnitBus alloc] initWithFormat:format error:outError];
    if (bus != nil && info.name != NULL) {
        bus.name = @(info.name);
    }
    return bus;
}

/// The declared topology, captured by value into the render block (blocks
/// cannot capture bare C arrays). Immutable at runtime.
typedef struct {
    uint32_t inputBusCount;
    uint32_t outputBusCount;
    /// Declared channel count per bus, clamped to `kSkuizMaxBusChannels`.
    uint32_t inputChannels[kSkuizMaxBusesPerDirection];
    uint32_t outputChannels[kSkuizMaxBusesPerDirection];
} SkuizBusTopology;

/// The declared buses in one direction as an `AUAudioUnitBusArray`, or nil
/// on failure. A direction with no declared buses (an instrument's inputs)
/// yields an array with zero busses — an absent input, not a failure.
static AUAudioUnitBusArray *_Nullable SkuizCreateBusArray(AUAudioUnit *unit,
                                                          AUAudioUnitBusType busType,
                                                          uint8_t direction, NSError **outError) {
    uint32_t count = skuiz_auv3_audio_bus_count(direction);
    // validate_buses caps the count in Rust; refuse to init rather than
    // guess at a topology we cannot represent.
    if (count > kSkuizMaxBusesPerDirection) {
        return nil;
    }
    NSMutableArray<AUAudioUnitBus *> *busses = [NSMutableArray arrayWithCapacity:count];
    for (uint32_t b = 0; b < count; b++) {
        AUAudioUnitBus *bus = SkuizCreateBus(direction, b, outError);
        if (bus == nil) {
            return nil;
        }
        [busses addObject:bus];
    }
    return [[AUAudioUnitBusArray alloc] initWithAudioUnit:unit busType:busType busses:busses];
}

/// Allocate the sidechain pull targets: one buffer list per declared input
/// past the main bus, sized to `maxFrames`. Main thread only.
static SkuizPullStorage *_Nullable SkuizAllocPullStorage(uint32_t maxFrames) {
    uint32_t inputCount = skuiz_auv3_audio_bus_count(0);
    if (inputCount <= 1 || inputCount > kSkuizMaxBusesPerDirection || maxFrames == 0) {
        return NULL;
    }
    SkuizPullStorage *storage = calloc(1, sizeof(SkuizPullStorage));
    if (storage == NULL) {
        return NULL;
    }
    storage->capacity = maxFrames;
    for (uint32_t b = 1; b < inputCount; b++) {
        SkuizAudioBusInfo info;
        if (!skuiz_auv3_audio_bus_info(0, b, &info) || info.channelCount == 0 ||
            info.channelCount > kSkuizMaxBusChannels) {
            continue;
        }
        size_t listSize = sizeof(AudioBufferList) + (info.channelCount - 1) * sizeof(AudioBuffer);
        AudioBufferList *list = calloc(1, listSize);
        float *data = calloc((size_t)info.channelCount * maxFrames, sizeof(float));
        if (list == NULL || data == NULL) {
            free(list);
            free(data);
            continue;
        }
        list->mNumberBuffers = info.channelCount;
        for (uint32_t c = 0; c < info.channelCount; c++) {
            list->mBuffers[c] = (AudioBuffer){.mNumberChannels = 1,
                                              .mDataByteSize = maxFrames * sizeof(float),
                                              .mData = data + (size_t)c * maxFrames};
        }
        storage->lists[b] = list;
        storage->data[b] = data;
    }
    return storage;
}

static void SkuizFreePullStorage(SkuizPullStorage *_Nullable storage) {
    if (storage == NULL) {
        return;
    }
    for (uint32_t b = 0; b < kSkuizMaxBusesPerDirection; b++) {
        free(storage->lists[b]);
        free(storage->data[b]);
    }
    free(storage);
}

@implementation SkuizAudioUnit {
    void *_instance;
    AUAudioUnitBusArray *_inputBusArray;
    AUAudioUnitBusArray *_outputBusArray;
    AUParameterTree *_parameterTree;
    /// Sidechain pull targets, allocated with the render resources. Main
    /// thread only; the render block captures the pointer.
    SkuizPullStorage *_pullStorage;
}

+ (nullable NSString *)skuizAppGroupDirectory {
    return gSkuizAppGroupDirectory;
}

+ (void)setSkuizAppGroupDirectory:(nullable NSString *)directory {
    gSkuizAppGroupDirectory = [directory copy];
}

- (instancetype)initWithComponentDescription:(AudioComponentDescription)componentDescription
                                     options:(AudioComponentInstantiationOptions)options
                                       error:(NSError **)outError {
    self = [super initWithComponentDescription:componentDescription options:options error:outError];
    if (self == nil) {
        return nil;
    }

    _instance = skuiz_auv3_init(gSkuizAppGroupDirectory.UTF8String);
    if (_instance == NULL) {
        if (outError) {
            *outError = [NSError errorWithDomain:NSOSStatusErrorDomain
                                            code:kAudioUnitErr_FailedInitialization
                                        userInfo:nil];
        }
        return nil;
    }

    // The declared bus topology is the single source of truth; translate it
    // into the AUAudioUnit bus model. An instrument declares no input buses
    // and gets a zero-count input array.
    _inputBusArray = SkuizCreateBusArray(self, AUAudioUnitBusTypeInput, 0, outError);
    _outputBusArray = SkuizCreateBusArray(self, AUAudioUnitBusTypeOutput, 1, outError);
    if (_inputBusArray == nil || _outputBusArray == nil) {
        return nil;
    }

    [self skuizBuildParameterTree];
    self.maximumFramesToRender = 4096;
    return self;
}

- (void)dealloc {
    SkuizFreePullStorage(_pullStorage);
    _pullStorage = NULL;
    if (_instance != NULL) {
        skuiz_auv3_destroy(_instance);
        _instance = NULL;
    }
}

#pragma mark - Parameters

- (void)skuizBuildParameterTree {
    uint32_t count = skuiz_auv3_param_count();
    NSMutableArray<AUParameter *> *parameters = [NSMutableArray arrayWithCapacity:count];

    for (uint32_t i = 0; i < count; i++) {
        SkuizParamInfo info;
        if (!skuiz_auv3_param_info(i, &info)) {
            continue;
        }
        NSString *name = info.name ? @(info.name) : @"";

        // A choice parameter becomes an indexed AUParameter carrying its
        // labels, which is what makes hosts draw a menu instead of a slider.
        NSMutableArray<NSString *> *valueStrings = nil;
        AudioUnitParameterOptions flags = kAudioUnitParameterFlag_IsReadable |
                                          kAudioUnitParameterFlag_IsWritable;
        AudioUnitParameterUnit unit = kAudioUnitParameterUnit_Generic;
        if (info.choiceCount > 0) {
            valueStrings = [NSMutableArray arrayWithCapacity:info.choiceCount];
            for (uint32_t c = 0; c < info.choiceCount; c++) {
                const char *label = skuiz_auv3_choice_label(info.id, c);
                [valueStrings addObject:label ? @(label) : @""];
            }
            flags |= kAudioUnitParameterFlag_ValuesHaveStrings;
            unit = kAudioUnitParameterUnit_Indexed;
        }

        AUParameter *parameter =
            [AUParameterTree createParameterWithIdentifier:[NSString stringWithFormat:@"p%u", info.id]
                                                      name:name
                                                   address:(AUParameterAddress)info.id
                                                       min:(AUValue)info.min
                                                       max:(AUValue)info.max
                                                      unit:unit
                                                  unitName:nil
                                                     flags:flags
                                              valueStrings:valueStrings
                                       dependentParameters:nil];
        parameter.value = (AUValue)info.defaultValue;
        [parameters addObject:parameter];
    }

    _parameterTree = [AUParameterTree createTreeWithChildren:parameters];

    // Capture the instance pointer, never self: these blocks outlive scope
    // and a strong self-capture would be a retain cycle.
    void *instance = _instance;
    _parameterTree.implementorValueObserver = ^(AUParameter *param, AUValue value) {
        // This assumes hosts fire value observers off the render thread
        // (the documented behaviour): skuiz_auv3_set_param allocates and
        // writes to the IPC socket, which has no place in a render callback.
        skuiz_auv3_set_param(instance, (uint32_t)param.address, (double)value);
    };
    _parameterTree.implementorValueProvider = ^AUValue(AUParameter *param) {
        return (AUValue)skuiz_auv3_get_param(instance, (uint32_t)param.address);
    };
    _parameterTree.implementorStringFromValueCallback = ^NSString *(AUParameter *param,
                                                                   const AUValue *valuePtr) {
        AUValue value = valuePtr ? *valuePtr : param.value;
        NSArray<NSString *> *strings = param.valueStrings;
        if (strings.count > 0) {
            NSInteger index = lroundf(value);
            if (index >= 0 && index < (NSInteger)strings.count) {
                return strings[index];
            }
        }
        return [NSString stringWithFormat:@"%.3f", value];
    };
}

- (AUParameterTree *)parameterTree {
    return _parameterTree;
}

#pragma mark - Busses

- (AUAudioUnitBusArray *)inputBusses {
    return _inputBusArray;
}

- (AUAudioUnitBusArray *)outputBusses {
    return _outputBusArray;
}

#pragma mark - State

- (NSDictionary<NSString *, id> *)fullState {
    NSMutableDictionary *state = [[super fullState] mutableCopy] ?: [NSMutableDictionary dictionary];
    // Ask for the size first, then fill a buffer of exactly that size.
    uint32_t needed = skuiz_auv3_save_state(_instance, NULL, 0);
    if (needed > 0) {
        NSMutableData *data = [NSMutableData dataWithLength:needed];
        uint32_t written = skuiz_auv3_save_state(_instance, data.mutableBytes, needed);
        if (written == needed) {
            state[kSkuizStateKey] = data;
        }
    }
    return state;
}

- (void)setFullState:(NSDictionary<NSString *, id> *)fullState {
    [super setFullState:fullState];
    NSData *data = fullState[kSkuizStateKey];
    if ([data isKindOfClass:[NSData class]] && data.length > 0) {
        skuiz_auv3_load_state(_instance, data.bytes, (uint32_t)data.length);
    }
}

#pragma mark - Rendering

- (BOOL)allocateRenderResourcesAndReturnError:(NSError **)outError {
    if (![super allocateRenderResourcesAndReturnError:outError]) {
        return NO;
    }
    // Sidechain pull targets must exist before the render block can run;
    // the render thread must not allocate.
    SkuizFreePullStorage(_pullStorage);
    _pullStorage = SkuizAllocPullStorage((uint32_t)self.maximumFramesToRender);
    // The main output's format drives the engine; a topology without
    // outputs falls back to the default rate.
    AUAudioUnitBus *mainOutput = _outputBusArray.count > 0 ? _outputBusArray[0] : nil;
    double sampleRate = mainOutput != nil ? mainOutput.format.sampleRate : 44100.0;
    skuiz_auv3_activate(_instance, sampleRate, (uint32_t)self.maximumFramesToRender);
    return YES;
}

- (void)deallocateRenderResources {
    // The pair of allocateRenderResourcesAndReturnError's activate.
    skuiz_auv3_deactivate(_instance);
    // Hosts serialise this against render calls, so the captured storage
    // pointer the render block holds is dead by now.
    SkuizFreePullStorage(_pullStorage);
    _pullStorage = NULL;
    [super deallocateRenderResources];
}

- (void)reset {
    // Host-initiated DSP reset (transport jump, recycled unit): routed
    // through the engine so it lands between blocks, never mid-buffer.
    skuiz_auv3_reset(_instance);
    [super reset];
}

- (NSArray<NSString *> *)MIDIOutputNames {
    return @[ @"MIDI Out" ];
}

- (AUInternalRenderBlock)internalRenderBlock {
    void *instance = _instance;
    // Captured now, not read inside the block: reading a property on the
    // render thread would message an Objective-C object mid-callback. Hosts
    // install this before asking for the render block.
    AUMIDIOutputEventBlock midiOut = self.MIDIOutputEventBlock;
    // Sidechain pull targets, allocated with the render resources. A block
    // fetched before allocation captures NULL and renders with sidechains
    // inactive.
    SkuizPullStorage *pullStorage = _pullStorage;
    // The declared topology, captured by value: the Rust side clamps to
    // these too, and the topology cannot change at runtime.
    SkuizBusTopology topology = {0};
    topology.inputBusCount = skuiz_auv3_audio_bus_count(0);
    topology.outputBusCount = skuiz_auv3_audio_bus_count(1);
    for (uint32_t b = 0; b < topology.inputBusCount && b < kSkuizMaxBusesPerDirection; b++) {
        SkuizAudioBusInfo info;
        if (skuiz_auv3_audio_bus_info(0, b, &info)) {
            topology.inputChannels[b] = MIN(info.channelCount, kSkuizMaxBusChannels);
        }
    }
    for (uint32_t b = 0; b < topology.outputBusCount && b < kSkuizMaxBusesPerDirection; b++) {
        SkuizAudioBusInfo info;
        if (skuiz_auv3_audio_bus_info(1, b, &info)) {
            topology.outputChannels[b] = MIN(info.channelCount, kSkuizMaxBusChannels);
        }
    }

    return ^AUAudioUnitStatus(AudioUnitRenderActionFlags *actionFlags,
                              const AudioTimeStamp *timestamp,
                              AUAudioFrameCount frameCount,
                              NSInteger outputBusNumber,
                              AudioBufferList *outputData,
                              const AURenderEvent *realtimeEventListHead,
                              AURenderPullInputBlock pullInputBlock) {
        if (outputData == NULL) {
            return kAudioUnitErr_InvalidParameter;
        }

        // Parameter events the host scheduled for this block, collected and
        // sorted by sample offset so the block can be split at event times
        // and automation lands sample-accurately. A ramp event is applied
        // stepwise at its offset — per-sample interpolation, and the
        // cross-block state it needs, is a ponytail.
        SkuizParamEvent timed[kSkuizMaxParamEvents];
        uint32_t timedCount = 0;
        for (const AURenderEvent *event = realtimeEventListHead; event != NULL;
             event = event->head.next) {
            if (event->head.eventType != AURenderEventParameter &&
                event->head.eventType != AURenderEventParameterRamp) {
                continue;
            }
            if (timedCount >= kSkuizMaxParamEvents) {
                break; // full: drop the excess, same philosophy as MidiOut
            }
            int64_t offset = event->head.eventSampleTime - (int64_t)timestamp->mSampleTime;
            if (offset < 0) {
                offset = 0;
            }
            if (offset > (int64_t)frameCount) {
                offset = (int64_t)frameCount;
            }
            // Insertion sort by frame; the list is tiny and fixed-size, and
            // hosts are documented to deliver it time-ordered anyway.
            uint32_t i = timedCount++;
            while (i > 0 && timed[i - 1].frame > (uint32_t)offset) {
                timed[i] = timed[i - 1];
                i--;
            }
            timed[i].frame = (uint32_t)offset;
            timed[i].address = (uint32_t)event->parameter.parameterAddress;
            timed[i].value = (double)event->parameter.value;
        }

        // Pull the upstream audio straight into the output buffers, then
        // process in place — the same shape every other Skuiz adapter uses.
        // The main input aliases the main output on the Rust side; only
        // sidechain buses get pointers of their own.
        if (pullInputBlock != NULL) {
            AudioUnitRenderActionFlags pullFlags = 0;
            AUAudioUnitStatus status =
                pullInputBlock(&pullFlags, timestamp, frameCount, 0, outputData);
            if (status != noErr) {
                return status;
            }
        } else {
            for (UInt32 i = 0; i < outputData->mNumberBuffers; i++) {
                if (outputData->mBuffers[i].mData != NULL) {
                    memset(outputData->mBuffers[i].mData, 0,
                           outputData->mBuffers[i].mDataByteSize);
                }
            }
        }

        SkuizAudioBusBuffers inputs[kSkuizMaxBusesPerDirection] = {0};
        SkuizAudioBusBuffers outputs[kSkuizMaxBusesPerDirection] = {0};

        // The main output bus is the one non-negotiable piece: a null
        // channel pointer here is a host bug and stays a hard error.
        uint32_t mainChannels =
            MIN((uint32_t)outputData->mNumberBuffers, topology.outputChannels[0]);
        for (uint32_t i = 0; i < mainChannels; i++) {
            float *channel = (float *)outputData->mBuffers[i].mData;
            if (channel == NULL) {
                return kAudioUnitErr_InvalidParameter;
            }
            outputs[0].channels[i] = channel;
        }
        outputs[0].channelCount = mainChannels;
        outputs[0].active = mainChannels > 0 ? 1 : 0;

        // Sidechain inputs: pull each declared bus past the main one, but
        // only when the host connected it. A failed or impossible pull
        // leaves the bus inactive — it is optional, never an error. A block
        // larger than the preallocated pull targets also renders with
        // sidechains inactive rather than overrunning the buffers.
        if (pullInputBlock != NULL && pullStorage != NULL &&
            frameCount <= pullStorage->capacity) {
            for (uint32_t b = 1;
                 b < topology.inputBusCount && b < kSkuizMaxBusesPerDirection; b++) {
                AudioBufferList *list = pullStorage->lists[b];
                if (list == NULL) {
                    continue;
                }
                // Re-point at our buffers: a host may have substituted its
                // own mData pointers on the previous pull.
                for (uint32_t c = 0; c < topology.inputChannels[b]; c++) {
                    list->mBuffers[c].mNumberChannels = 1;
                    list->mBuffers[c].mDataByteSize = pullStorage->capacity * sizeof(float);
                    list->mBuffers[c].mData =
                        pullStorage->data[b] + (size_t)c * pullStorage->capacity;
                }
                AudioUnitRenderActionFlags pullFlags = 0;
                AUAudioUnitStatus status =
                    pullInputBlock(&pullFlags, timestamp, frameCount, (NSInteger)b, list);
                if (status != noErr) {
                    continue;
                }
                uint32_t count = MIN((uint32_t)list->mNumberBuffers, topology.inputChannels[b]);
                uint32_t used = 0;
                for (uint32_t c = 0; c < count; c++) {
                    float *channel = (float *)list->mBuffers[c].mData;
                    if (channel == NULL) {
                        break;
                    }
                    inputs[b].channels[c] = channel;
                    used++;
                }
                inputs[b].channelCount = used;
                inputs[b].active = used > 0 ? 1 : 0;
            }
        }

        // Split the block at event times: render up to the next event,
        // apply it, continue. The render-safe setter allocates nothing and
        // does not broadcast — host automation is not shared over IPC,
        // matching the other adapters.
        uint32_t pos = 0;
        for (uint32_t i = 0; i < timedCount; i++) {
            if (timed[i].frame > pos) {
                SkuizRenderSegment(instance, inputs, outputs, pos, timed[i].frame,
                                   midiOut, timestamp);
                pos = timed[i].frame;
            }
            skuiz_auv3_set_param_from_render(instance, timed[i].address, timed[i].value);
        }
        if (pos < (uint32_t)frameCount) {
            SkuizRenderSegment(instance, inputs, outputs, pos, (uint32_t)frameCount,
                               midiOut, timestamp);
        }
        return noErr;
    };
}

@end

#pragma mark - Self test

/// Renders through a real AUAudioUnit without a host, an extension bundle or
/// code signing, which is what lets this shim be tested rather than merely
/// compiled.
int skuiz_auv3_selftest(void) {
    @autoreleasepool {
        AudioComponentDescription description = {
            .componentType = kAudioUnitType_Effect,
            .componentSubType = 'skuz',
            .componentManufacturer = 'Skuz',
            .componentFlags = 0,
            .componentFlagsMask = 0,
        };
        NSError *error = nil;
        SkuizAudioUnit *unit = [[SkuizAudioUnit alloc] initWithComponentDescription:description
                                                                            options:0
                                                                              error:&error];
        if (unit == nil) {
            return 1;
        }
        if (unit.parameterTree.allParameters.count != skuiz_auv3_param_count()) {
            return 2;
        }

        // Parameters must travel through the tree into Rust and back.
        AUParameter *first = unit.parameterTree.allParameters.firstObject;
        if (first == nil) {
            return 3;
        }
        first.value = 0.25f;
        if (fabs(first.value - 0.25f) > 1e-4) {
            return 4;
        }

        if (![unit allocateRenderResourcesAndReturnError:&error]) {
            return 5;
        }

        // The declared topology must surface as bus arrays: stereo main in,
        // an optional mono sidechain in, stereo main out.
        if (unit.inputBusses.count != 2 || unit.outputBusses.count != 1) {
            return 18;
        }
        AUAudioUnitBus *mainIn = unit.inputBusses[0];
        AUAudioUnitBus *sidechainBus = unit.inputBusses[1];
        if (mainIn.format.channelCount != 2 || ![mainIn.name isEqualToString:@"Main"] ||
            sidechainBus.format.channelCount != 1 ||
            ![sidechainBus.name isEqualToString:@"Sidechain"]) {
            return 19;
        }

        // Render a block of ones and check the processor changed it. With
        // the fixture's gain at 0.25 the output must come back quartered.
        const AUAudioFrameCount frames = 128;
        float left[128], right[128];
        for (AUAudioFrameCount i = 0; i < frames; i++) {
            left[i] = 1.0f;
            right[i] = 1.0f;
        }
        char storage[sizeof(AudioBufferList) + sizeof(AudioBuffer)];
        AudioBufferList *bufferList = (AudioBufferList *)storage;
        bufferList->mNumberBuffers = 2;
        bufferList->mBuffers[0] = (AudioBuffer){.mNumberChannels = 1,
                                                .mDataByteSize = frames * sizeof(float),
                                                .mData = left};
        bufferList->mBuffers[1] = (AudioBuffer){.mNumberChannels = 1,
                                                .mDataByteSize = frames * sizeof(float),
                                                .mData = right};

        AudioTimeStamp timestamp = {0};
        timestamp.mSampleTime = 0;
        timestamp.mFlags = kAudioTimeStampSampleTimeValid;
        AudioUnitRenderActionFlags flags = 0;
        AUInternalRenderBlock render = unit.internalRenderBlock;
        // No pull block: the shim zeroes the buffers, so feed the signal by
        // rendering once and then writing into the buffers it cleared.
        AUAudioUnitStatus status = render(&flags, &timestamp, frames, 0, bufferList, NULL, NULL);
        if (status != noErr) {
            return 6;
        }
        // Silence in, so silence out regardless of gain.
        for (AUAudioFrameCount i = 0; i < frames; i++) {
            if (left[i] != 0.0f) {
                return 7;
            }
        }

        // Now with an input: a pull block supplying ones. Only the main bus
        // is connected — the fixture declares a sidechain too, and a host
        // that never connected it fails the pull, which the shim must treat
        // as an inactive bus, not an error.
        AURenderPullInputBlock pull =
            ^AUAudioUnitStatus(AudioUnitRenderActionFlags *pullFlags, const AudioTimeStamp *ts,
                               AUAudioFrameCount count, NSInteger bus, AudioBufferList *data) {
                if (bus != 0) {
                    return kAudioUnitErr_InvalidElement;
                }
                for (UInt32 b = 0; b < data->mNumberBuffers; b++) {
                    float *samples = (float *)data->mBuffers[b].mData;
                    for (AUAudioFrameCount i = 0; i < count; i++) {
                        samples[i] = 1.0f;
                    }
                }
                return noErr;
            };
        status = render(&flags, &timestamp, frames, 0, bufferList, NULL, pull);
        if (status != noErr) {
            return 8;
        }
        for (AUAudioFrameCount i = 0; i < frames; i++) {
            if (fabsf(left[i] - 0.25f) > 1e-5f || fabsf(right[i] - 0.25f) > 1e-5f) {
                return 9;
            }
        }

        // A parameter event scheduled mid-block must take effect at its
        // frame: frames before it keep the current gain, frames after it
        // use the new one.
        AURenderEvent paramEvent;
        memset(&paramEvent, 0, sizeof(paramEvent));
        paramEvent.head.eventSampleTime = frames / 2;
        paramEvent.head.eventType = AURenderEventParameter;
        paramEvent.parameter.parameterAddress = first.address;
        paramEvent.parameter.value = 0.5f;
        status = render(&flags, &timestamp, frames, 0, bufferList, &paramEvent, pull);
        if (status != noErr) {
            return 14;
        }
        for (AUAudioFrameCount i = 0; i < frames; i++) {
            float expect = i < frames / 2 ? 0.25f : 0.5f;
            if (fabsf(left[i] - expect) > 1e-5f || fabsf(right[i] - expect) > 1e-5f) {
                return 15;
            }
        }
        // The scheduled value must stick for subsequent renders: render
        // again with no events and the whole block comes back at 0.5.
        // (Not via `first.value`: the render-safe setter deliberately
        // doesn't notify the parameter tree.)
        status = render(&flags, &timestamp, frames, 0, bufferList, NULL, pull);
        if (status != noErr) {
            return 16;
        }
        for (AUAudioFrameCount i = 0; i < frames; i++) {
            if (fabsf(left[i] - 0.5f) > 1e-5f) {
                return 17;
            }
        }
        // Back to 0.25 through the tree, so the state round-trip below
        // starts from a tree-coherent value (the render-safe setter above
        // deliberately left the tree unnotified).
        first.value = 0.25f;

        // MIDI the DSP produced must reach the host's output block.
        __block int midiSeen = 0;
        __block uint8_t midiStatus = 0;
        unit.MIDIOutputEventBlock = ^OSStatus(AUEventSampleTime when, uint8_t cable,
                                              NSInteger length, const uint8_t *data) {
            if (length == 3) {
                midiSeen++;
                midiStatus = data[0];
            }
            return noErr;
        };
        // Re-fetch: the block is captured when the render block is built.
        AUInternalRenderBlock renderWithMIDI = unit.internalRenderBlock;
        status = renderWithMIDI(&flags, &timestamp, frames, 0, bufferList, NULL, pull);
        if (status != noErr) {
            return 12;
        }
        if (midiSeen != 1 || (midiStatus & 0xF0) != 0x90) {
            return 13;
        }

        // With the sidechain connected, the DSP must see it: the fixture
        // writes 100 + sidechain[0] into frame 0 when the bus is active.
        // Every render above went through a pull block that refuses bus 1,
        // so their plain-gain results already prove an unconnected
        // sidechain stays inactive.
        AURenderPullInputBlock pullWithSidechain =
            ^AUAudioUnitStatus(AudioUnitRenderActionFlags *pullFlags, const AudioTimeStamp *ts,
                               AUAudioFrameCount count, NSInteger bus, AudioBufferList *data) {
                float fill = bus == 0 ? 1.0f : 0.5f;
                for (UInt32 b = 0; b < data->mNumberBuffers; b++) {
                    float *samples = (float *)data->mBuffers[b].mData;
                    for (AUAudioFrameCount i = 0; i < count; i++) {
                        samples[i] = fill;
                    }
                }
                return noErr;
            };
        status = render(&flags, &timestamp, frames, 0, bufferList, NULL, pullWithSidechain);
        if (status != noErr) {
            return 20;
        }
        // Gain is 0.25 here: frame 0 carries the marker, the rest is gain.
        if (fabsf(left[0] - 100.5f) > 1e-4f || fabsf(left[1] - 0.25f) > 1e-5f) {
            return 21;
        }
        // And a later block without the sidechain must see it gone again.
        status = render(&flags, &timestamp, frames, 0, bufferList, NULL, pull);
        if (status != noErr) {
            return 20;
        }
        if (fabsf(left[0] - 0.25f) > 1e-5f) {
            return 21;
        }

        // State must survive a save/load cycle through fullState.
        NSDictionary *saved = unit.fullState;
        if (![saved[kSkuizStateKey] isKindOfClass:[NSData class]]) {
            return 10;
        }
        first.value = 0.9f;
        unit.fullState = saved;
        if (fabs(first.value - 0.25f) > 1e-4) {
            return 11;
        }

        // The deactivate half of the lifecycle must run cleanly too.
        [unit deallocateRenderResources];

        return 0;
    }
}
