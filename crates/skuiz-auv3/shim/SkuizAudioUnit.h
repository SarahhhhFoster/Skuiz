//  SkuizAudioUnit.h — the Objective-C half of a Skuiz AUv3 plugin.
//
//  This is the AUAudioUnit subclass an Audio Unit app extension names as its
//  principal class. It owns no DSP or state: every call is forwarded to the
//  `skuiz_auv3_*` C ABI that `skuiz_auv3::export_auv3!` generates in the
//  plugin crate, so the plugin behaves identically here and under CLAP,
//  VST3, or the standalone shell.

#ifndef SKUIZ_AUDIO_UNIT_H
#define SKUIZ_AUDIO_UNIT_H

#import <AudioToolbox/AudioToolbox.h>

NS_ASSUME_NONNULL_BEGIN

@interface SkuizAudioUnit : AUAudioUnit

/// Directory for the cross-process bus socket, or nil for the sandbox's own
/// temp directory.
///
/// Instances inside one host share a single extension process and sync by
/// direct call, so nil is correct unless you also want to sync with
/// instances in *other* hosts. For that, set this before the unit is
/// instantiated — the value is consumed in `initWithComponentDescription`,
/// so a `+load` method or another early initialiser is the reliable place —
/// to the App Group container from
/// `-[NSFileManager containerURLForSecurityApplicationGroupIdentifier:]`.
@property (class, nonatomic, copy, nullable) NSString *skuizAppGroupDirectory;

@end

/// Exercises the shim end to end in-process: builds a unit, allocates render
/// resources, renders a block, and round-trips a parameter and the state
/// blob. Returns 0 on success, or a non-zero code identifying the step that
/// failed. Useful when wiring up an Xcode target, and it is what the Rust
/// test suite runs.
int skuiz_auv3_selftest(void);

NS_ASSUME_NONNULL_END

#endif
