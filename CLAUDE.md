# GTK Status Bar - Claude Code Assistant Configuration

## Error Handling Philosophy

While complete elimination of unwrap() calls is ideal, the primary focus is on
comprehensive error handling with appropriate logging and graceful degradation.
Every Result and Option type should be explicitly handled with meaningful error
messages and fallback behavior. When unwrap() is used, it should be justified
with comments explaining why the operation cannot fail in the given context.

Functions should prioritize error condition handling before success scenarios.
This approach keeps the main logic flow clear and ensures edge cases are
properly addressed. Early returns for error conditions reduce nesting and
improve code readability while maintaining comprehensive error coverage.

## Code Organization and Readability Principles

Code should be structured to minimize the reader's cognitive load and avoid
excessive eye movement to understand the control flow. Several patterns help
achieve this:

**Happy Path Prioritization**: Short error/unhappy path code blocks should
precede long happy path code blocks. This allows readers to quickly understand
edge cases before diving into the main logic.

**Match Statements for Structure Handling**: Use match statements to handle
nested structures instead of deeply nested if-let chains. Match statements
provide clear visual separation of cases and avoid excessive indentation levels.

**Guard Clauses and Early Returns**: Prefer `let else` statements and other
guard patterns that handle error conditions early and keep the happy path at the
top level of indentation. This flattens control flow and improves readability.

```rust
// Preferred: Guard clause with let else
let Ok(Some(Value::Dict(device_props))) = interfaces.get("org.bluez.Device1") else {
    debug!("Device1 interface not found");
    return;
};

// Preferred: Match for structured data handling
match device_props.get("Name") {
    Ok(Some(Value::Str(name))) => {
        debug!("Found device name: {}", name);
        // Happy path logic here
    },
    Ok(Some(other)) => {
        error!("Name property has unexpected type: {:?}", other);
    },
    Ok(None) => {
        error!("Device1 interface found but no Name property");
    },
    Err(e) => {
        error!("Failed to get Name property: {}", e);
    }
}
```

**Combinators with Caution**: While combinators like `and_then` and `map` are
acceptable, they are less flexible than match statements for complex error
handling. Use them for simple transformations but prefer match for comprehensive
case handling.

## Logging and Layer Unwrapping

We emphasize thorough logging to validate all assumptions during development and
debugging. When working with nested structures or multiple layers of
abstraction, unwrap them one layer at a time with logging between each step.

**Assumption Validation**: Log intermediate states to verify that data
structures contain expected values. This helps identify issues early and
provides debugging context.

**Progressive Unwrapping**: Instead of chaining multiple operations, break them
into discrete steps with logging:

```rust
// Preferred: Step-by-step unwrapping with logging
match interfaces_and_properties.get::<_, Value>(&bluetooth_interface_key) {
    Ok(Some(Value::Dict(device1))) => {
        debug!("Found Device1 interface properties: {:?}", device1);

        match device1.get(&zvariant::Str::from("Name")) {
            Ok(Some(Value::Str(name))) => {
                debug!("Found Bluetooth device name: {}", name);
                // Process name...
            },
            Ok(Some(other)) => {
                error!("Device Name property has unexpected type: {:?}", other);
            },
            // ... handle other cases
        }
    },
    Ok(Some(other)) => {
        error!("Device1 interface found but has unexpected type: {:?}", other);
    },
    // ... handle other cases
}
```

## Locality of Behavior

Locality of behavior is preferred over aggressive function extraction. Code
should be organized to maintain clear, readable control flow while keeping
related logic together. When control flow becomes complex, consider
restructuring to improve understanding without necessarily breaking into
multiple functions.

## Logging and Observability

Comprehensive logging using the tracing crate provides structured observability
throughout the application. Every significant code path should include
appropriate log levels with contextual information for debugging and monitoring.
Performance-critical operations should use trace-level logging to avoid
impacting runtime performance.

Error conditions require detailed logging with sufficient context for diagnosis.
This includes the operation being attempted, input parameters where relevant,
and the specific failure mode encountered. Warning-level logs should indicate
recoverable issues that may require attention.
