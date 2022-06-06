package co.bluey;

import android.bluetooth.BluetoothDevice;
import android.bluetooth.BluetoothGatt;
import android.bluetooth.BluetoothGattCharacteristic;
import android.bluetooth.BluetoothGattDescriptor;

import java.util.HashMap;

public class BleDevice {
    public BluetoothDevice device = null;
    public long nativeHandle = 0;
    public BluetoothGatt connectedGatt = null;
    private HashMap<BluetoothGattDescriptor, Integer> descriptorIds = new HashMap<BluetoothGattDescriptor, Integer>();
    private int nextDescriptorHandle = 1;

    public BleDevice(BluetoothDevice device, long nativeHandle) {
        this.device = device;
        this.nativeHandle = nativeHandle;
    }

    public synchronized int getDescriptorId(BluetoothGattDescriptor descriptor) {
        Integer handle = descriptorIds.get(descriptor);
        if (handle == null) {
            int newHandle = nextDescriptorHandle++;
            descriptorIds.put(descriptor, newHandle);
            return newHandle;
        }
        return handle;
    }

    //public synchronized void addDescriptorHandle(BluetoothGattDescriptor descriptor, long handle) {
    //    descriptorHandles.put(descriptor, handle);
    //}

    // XXX: actually we currently assume that each new connection will
    // instantiate a new BleDevice so we don't have to worry about
    // cleaning up old descriptor Ids...
    public synchronized void clearGattState() {
        descriptorIds.clear();
    }
}
