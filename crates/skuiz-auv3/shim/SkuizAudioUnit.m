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

extern void *skuiz_auv3_init(const char *appGroupDir);
extern void skuiz_auv3_destroy(void *inst);
extern void skuiz_auv3_activate(void *inst, double sampleRate, uint32_t maxFrames);
extern void skuiz_auv3_deactivate(void *inst);
extern void skuiz_auv3_render(void *inst, float *const *channels,
                              uint32_t channelCount, uint32_t frames);
extern uint32_t skuiz_auv3_param_count(void);
extern bool skuiz_auv3_param_info(uint32_t index, SkuizParamInfo *out);
extern const char *skuiz_auv3_choice_label(uint32_t paramID, uint32_t choiceIndex);
extern double skuiz_auv3_get_param(void *inst, uint32_t id);
extern void skuiz_auv3_set_param(void *inst, uint32_t id, double value);
extern void skuiz_auv3_set_param_from_render(void *inst, uint32_t id, double value);
extern uint32_t skuiz_auv3_save_state(void *inst, uint8_t *buf, uint32_t cap);
extern bool skuiz_auv3_load_state(void *inst, const uint8_t *buf, uint32_t len);
extern uint32_t skuiz_auv3_midi_count(void *inst);
extern bool skuiz_auv3_midi_event(void *inst, uint32_t index, uint32_t *frame, uint8_t *bytes3);

/// Channels the render path handles, matching the Processor contract.
#define kSkuizMaxChannels 2u
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

/// Render `channels[start..end]` in place and hand the segment's MIDI to the
/// host, offset back into block time. Called once per automation segment, so
/// MIDI frames the DSP stamps are segment-relative.
static void SkuizRenderSegment(void *instance, float *const *channels, uint32_t channelCount,
                               uint32_t start, uint32_t end, AUMIDIOutputEventBlock midiOut,
                               const AudioTimeStamp *timestamp) {
    float *segment[kSkuizMaxChannels] = {NULL, NULL};
    for (uint32_t i = 0; i < channelCount; i++) {
        segment[i] = channels[i] + start;
    }
    skuiz_auv3_render(instance, segment, channelCount, end - start);

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

@implementation SkuizAudioUnit {
    void *_instance;
    AUAudioUnitBus *_inputBus;
    AUAudioUnitBus *_outputBus;
    AUAudioUnitBusArray *_inputBusArray;
    AUAudioUnitBusArray *_outputBusArray;
    AUParameterTree *_parameterTree;
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

    AVAudioFormat *format = [[AVAudioFormat alloc] initStandardFormatWithSampleRate:44100.0
                                                                          channels:kSkuizMaxChannels];
    _inputBus = [[AUAudioUnitBus alloc] initWithFormat:format error:outError];
    _outputBus = [[AUAudioUnitBus alloc] initWithFormat:format error:outError];
    if (_inputBus == nil || _outputBus == nil) {
        return nil;
    }
    _inputBusArray = [[AUAudioUnitBusArray alloc] initWithAudioUnit:self
                                                           busType:AUAudioUnitBusTypeInput
                                                            busses:@[ _inputBus ]];
    _outputBusArray = [[AUAudioUnitBusArray alloc] initWithAudioUnit:self
                                                            busType:AUAudioUnitBusTypeOutput
                                                             busses:@[ _outputBus ]];

    [self skuizBuildParameterTree];
    self.maximumFramesToRender = 4096;
    return self;
}

- (void)dealloc {
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
    skuiz_auv3_activate(_instance, _outputBus.format.sampleRate,
                        (uint32_t)self.maximumFramesToRender);
    return YES;
}

- (void)deallocateRenderResources {
    // The pair of allocateRenderResourcesAndReturnError's activate.
    skuiz_auv3_deactivate(_instance);
    [super deallocateRenderResources];
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

        float *channels[kSkuizMaxChannels] = {NULL, NULL};
        UInt32 channelCount = outputData->mNumberBuffers;
        if (channelCount > kSkuizMaxChannels) {
            channelCount = kSkuizMaxChannels;
        }
        for (UInt32 i = 0; i < channelCount; i++) {
            channels[i] = (float *)outputData->mBuffers[i].mData;
            if (channels[i] == NULL) {
                return kAudioUnitErr_InvalidParameter;
            }
        }

        // Split the block at event times: render up to the next event,
        // apply it, continue. The render-safe setter allocates nothing and
        // does not broadcast — host automation is not shared over IPC,
        // matching the other adapters.
        uint32_t pos = 0;
        for (uint32_t i = 0; i < timedCount; i++) {
            if (timed[i].frame > pos) {
                SkuizRenderSegment(instance, channels, channelCount, pos, timed[i].frame,
                                   midiOut, timestamp);
                pos = timed[i].frame;
            }
            skuiz_auv3_set_param_from_render(instance, timed[i].address, timed[i].value);
        }
        if (pos < (uint32_t)frameCount) {
            SkuizRenderSegment(instance, channels, channelCount, pos, (uint32_t)frameCount,
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

        // Now with an input: a pull block supplying ones.
        AURenderPullInputBlock pull =
            ^AUAudioUnitStatus(AudioUnitRenderActionFlags *pullFlags, const AudioTimeStamp *ts,
                               AUAudioFrameCount count, NSInteger bus, AudioBufferList *data) {
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
