# Media Model

## Time

Every video frame and audio block must carry a timestamp in an explicit rational time base. Use a monotonic clock for scheduling. Wall-clock time is for human-readable logs only.

## Video frame

The eventual frame type should include:

- presentation timestamp
- width and height
- pixel format / GPU texture format
- color space and range
- CPU buffer or GPU resource handle
- ownership/lifetime rules

Do not force all sources into CPU memory if a safe GPU import path exists. Start with a reliable CPU path where necessary, then optimize based on profiling.

## Audio block

The eventual audio block type should include:

- sample timestamp
- sample rate
- channel layout
- sample format
- frame/sample count
- buffer ownership

Audio processing should use predictable block sizes and avoid allocations in the real-time callback path.

## Synchronization

Audio and video use different production rates. Synchronize by timestamps, not by assuming one video frame corresponds to one audio block. Define behavior for late frames, missing audio, capture interruptions, and encoder backpressure.

## Queue policy

Each edge in the media graph must have a bounded capacity and an explicit overload policy. Live video may drop stale frames to protect latency. Audio overrun handling must be deliberate and measurable. Every drop/overrun increments a metric.
