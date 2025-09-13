# Bug Report: Volume Widget Not Updating on Bluetooth Disconnect

## Summary
The volume widget fails to update when Bluetooth audio devices disconnect, but correctly updates when they connect. This causes the widget to display stale volume information from the previously connected Bluetooth device.

## Environment
- **Application**: GTK Status Bar with PipeWire volume monitoring
- **Audio System**: PipeWire with Bluetooth audio support
- **Device**: TOZO-T10 Bluetooth headphones

## Expected Behavior
When Bluetooth audio devices disconnect, the volume widget should:
1. Detect the default sink change from Bluetooth device to built-in audio
2. Update the display to show the new default sink's volume
3. Change from `🔊T90` (TOZO-T10 at 90%) to `🔊B<volume>` (Built-in at current volume)

## Actual Behavior
When Bluetooth devices disconnect:
1. PipeWire correctly detects the default sink change: `🔄 Default sink -> alsa_output.pci-0000_00_1b.0.analog-stereo`
2. Volume widget display remains unchanged, showing stale Bluetooth device info: `🔊T90`
3. No volume update is sent to the GTK UI

## Steps to Reproduce
1. Connect Bluetooth audio device (TOZO-T10 headphones)
2. Verify volume widget shows: `🔊T90` (works correctly)
3. Disconnect Bluetooth device
4. Observe volume widget still shows: `🔊T90` (should update to built-in audio)

## Log Analysis

### Working Case - Connection
```
🔊 DEFAULT Node 7: TOZO-T10 - Vol: Some(100)% | Ch: Some(90)% | Mute: Some(false) [ASYNC DELIVERY]
📺 GTK UI updated via ASYNC: 🔊T90
```

### Failing Case - Disconnection  
```
🔄 Default sink -> alsa_output.pci-0000_00_1b.0.analog-stereo
❌ PipeWire error id:7 seq:11 res:-14: Received error event
metadata.property subject=0 key=Some("default.audio.sink") value=Some("{\"name\":\"alsa_output.pci-0000_00_1b.0.analog-stereo\"}")
```

**Missing**: No `📺 GTK UI updated via ASYNC` message after disconnect

## Root Cause Analysis
The metadata correctly detects the default sink change, but the volume monitoring system fails to:
1. Query the new default sink's volume properties
2. Send a `VolumeUpdate` to the GTK UI channel
3. This suggests the PipeWire node listener may not be triggered for the built-in audio device

## Technical Details
- **PipeWire Error**: `res:-14` suggests a resource/node unavailable error when the Bluetooth node is removed
- **Metadata Update**: Correctly receives new default sink metadata
- **Missing Volume Query**: No subsequent volume property fetch for the new default sink
- **UI Update Gap**: No `VolumeUpdate` sent through async channel after sink change

## Workaround
Manually triggering a volume change on the built-in audio device (via volume controls) forces the widget to update correctly.

## Priority
**Medium** - Affects user experience when switching between audio devices, but doesn't break core functionality.