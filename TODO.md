# TODO

## Battery Plug/Unplug Detection

### Current Status
- ✅ Basic State property detection implemented (logs charging/discharging)
- ✅ Correct D-Bus types fixed (F64 for Percentage, U32 for State) 
- ✅ Clean error handling pattern following Bluetooth implementation

### Missing Implementation

#### UI Updates for Battery State Changes
- [ ] Update battery widget text/icon based on charging state
- [ ] Show charging indicator (⚡) when State = 1 (Charging) or 4 (Fully charged)
- [ ] Show battery icon only (🔋) when State = 2 (Discharging)
- [ ] Handle other states: Empty (3), Pending charge (5), Pending discharge (6)
- [ ] Consider color changes or visual indicators for different states

#### Initial State Query at Program Start
- [ ] Query initial battery State property (similar to initial Percentage query)
- [ ] Use `process_battery_device_properties()` for initial state processing
- [ ] Ensure UI shows correct charging/discharging state on startup

#### Code Consistency and Clean Patterns
- [ ] Apply clean Bluetooth-style pattern to other D-Bus handlers
- [ ] Use explicit `Value::Type(value)` matching instead of `try_from()` conversions
- [ ] Separate error cases with specific log messages (Err/Ok(None)/Ok(Some))
- [ ] Extract property processing into reusable functions

#### Logging Improvements
- [ ] **Selective logging**: Add compile-time or runtime flags to reduce log verbosity
- [ ] **Timestamp reduction**: Option to disable timestamps for LLM-friendly output
- [ ] Consider structured logging levels for different components
- [ ] Add log filtering for D-Bus event spam vs useful state changes

#### Bluetooth Initialization
- [ ] Add initial Bluetooth device state query (currently missing)
- [ ] Query connected BT devices with battery interfaces on startup
- [ ] Apply same initialization pattern as battery monitoring

### Implementation Notes
- Follow the pattern established in `process_bluetooth_battery_interface()`
- Use shared processing functions for both initialization and event handling
- Keep logging granular but configurable for development vs production use