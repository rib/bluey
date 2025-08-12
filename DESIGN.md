# Portability considerations

In the process of porting to different platforms we will find design details that
will steer our internal design and public API to ensure to help provide more
consistency across platform.

# Windows

On Windows the Bluetooth LE API isn't explicitly connection oriented, like other
platforms; there is no "connect" API, although it does offer an API for notifying
you of connect/disconnect events. We can implicitly cause a connect by querying
GATT state that requires a connection.

# Android

There seems to be so many Bluetooth pitfalls on Android across different devices
from different vendors that may each have their own low-level bluetooth stacks
beneath the Java interface.

There's are lots of developer reports about needing to queue/serialize requests
into the bluetooth stack since the drivers are very likely to abort outstanding
work.

The bluetooth stack internally caches devices discovered when scanning and
some APIs will behave inconsistently if a device has not been cached already.
For example connectGatt with autoconnect = true seems to require that the
device has been cached.

It's not documented but in practice it sounds like connectGatt with
autoconnect = false can be expected to time out after 30 seconds if the device
isn't found.

It's not supported to issue multiple connectGatt, autoconnect = false requests
to different devices in parallel.

This (unanswered) stack overflow question:
    https://stackoverflow.com/questions/68521783/how-to-detect-android-bluetooth-connectgatt-auto-connect-completion
also implies that (at least on some devices) then attempts to start multiple
autoconnect = true connectGatt requests can cause earlier requests to abort unless
there is at least some small delay between starting
requests (they used 500ms) and there doesn't seem to be an objective way of knowing
how much throttling is required.

# Connection Request Lifecycle

The differences in how a GATT connect is initiated and completed across platforms
are quite significant....

Even though connections on Windows are triggered indirectly it's natural for us to
support an async connect() API that will complete when the underlying connection
either completes or fails.

With the asynchronous callbacks on Android the lifecycle of a connection request
is more complicated to track and it would be easier to support an API that is
documented to start/request a connection but applications must wait for a
PeripheralConnected event to determine when the request has completed.

Core Bluetooth is similar to Android in that 'connect' only starts/requests a
connection and applications need to wait for a notification about the connection
completing or about any error.

Separating the connect start/request and completion notification is the most
portable abstraction since the notification can be delivered immediately on
platforms where that's possible. Technically we could also provide a portable
future-based abstraction over connection requests to support awaiting completion
on all platforms but it's also worth noting that this complicates the possibility
of supporting cancellation of connection requests, and for applications that may
be handling multiple devices then tracking futures that could be waiting for
an unbounded amount of time for a connection to complete might be more complex
that relying on a callback/notification based approach instead.

Another important detail is the auto-reconnect behaviour that differs between
implementations. On Android you can choose whether to start an open-ended
or immediate connect (autoconnect = true/false). Core Bluetooth behaves more
like Android autoconnect = true. By default Windows behaves more like Android
autoconnect = false, but they have also introduced a GattSession that can
be retrieved from a GattService (implicitly after an initial connection)
and with that you can set MaintainConnection = true to get similar behaviour
to autoconnect = true.

# Issues

On Android we don't have a way of reporting an error status from trying to
read a peripheral's RSSI. It could make sense to expose the property similar
to Core Bluetooth instead, whereby there would a separate event when the
RSSI value is read which could include an error status if there was a
problem.

We don't have a clear way to disconnect peripherals, apart from dropping them.

No support for querying characteristic descriptors.

We should have an explicit asynchronous peripheral.read_rssi() API for
asynchronously readding the device RSSI. This would fit better with the
Android and Core Bluetooth APIs. Currently we only support tracing
advertised RSSI state while scanning, and can't query it explicitly.


# Peripheral State

Currently peripherals are identified via a 'handle' that is defined by the
backend (any u32 integer) and from the application point of view the handle
is wrapped along with a Session reference so that the Session is kept alive
while the application holds a reference to a peripheral.

It's notable that we don't refer to peripherals in the same way internally
since we don't want a cyclic dependency between peripherals and the session.

Notably though the peripherals are not currently reference counted for
applications - they just ensure the Session will be kept alive and uniquely
identify the device. This means we don't actually know when an application
no longer cares about a particular peripheral, and we can't e.g. automatically
disconnect or free cached state for peripherals that aren't needed anymore.


