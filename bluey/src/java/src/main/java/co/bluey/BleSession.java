package co.bluey;

import android.annotation.SuppressLint;
import android.app.Activity;
import android.bluetooth.BluetoothAdapter;
import android.bluetooth.BluetoothDevice;
import android.bluetooth.BluetoothGatt;
import android.bluetooth.BluetoothGattCallback;
import android.bluetooth.BluetoothGattCharacteristic;
import android.bluetooth.BluetoothGattDescriptor;
import android.bluetooth.BluetoothGattService;
import android.bluetooth.BluetoothManager;
import android.bluetooth.BluetoothProfile;
import android.bluetooth.le.BluetoothLeScanner;
import android.bluetooth.le.ScanCallback;
import android.bluetooth.le.ScanFilter;
import android.bluetooth.le.ScanResult;
import android.bluetooth.le.ScanSettings;
import android.companion.AssociationRequest;
import android.companion.BluetoothDeviceFilter;
import android.companion.CompanionDeviceManager;
import android.content.BroadcastReceiver;
import android.content.Context;
import android.content.Intent;
import android.content.IntentFilter;
import android.content.IntentSender;
import android.os.Build;
import android.os.Looper;
import android.os.ParcelUuid;
import android.util.Log;

//import androidx.annotation.RequiresApi;

import java.lang.ref.WeakReference;
import java.util.ArrayList;
import java.util.List;
import java.util.UUID;
import java.util.WeakHashMap;


// NB usefull references:
// https://android.googlesource.com/platform/frameworks/base/+/refs/heads/android10-release/core/java/android/bluetooth/BluetoothGatt.java
// https://android.googlesource.com/platform/packages/apps/Bluetooth/+/refs/heads/android10-mainline-release/src/com/android/bluetooth/gatt/GattService.java

public class BleSession /*implements Closeable*/ {
    private long sessionHandle = 0;

    private WeakReference<Activity> activity;
    public static int COMPANION_CHOOSER_REQUEST_CODE;
    //private BluetoothGatt mConnectedGatt = null;

    private BluetoothManager manager = null;
    private BluetoothAdapter adapter = null;

    // We only support a single session scanning at a time. This way we can simply
    // forward any companion responses to this session.
    private static BleSession scannerSession = null;
    BluetoothLeScanner scanner = null;

    private WeakHashMap<BluetoothDevice, Long> deviceHandles = new WeakHashMap<BluetoothDevice, Long>();

    public BleSession(Activity activity, int companionChooserRequestCode) {
        this.activity = new WeakReference<Activity>(activity);
        BleSession.COMPANION_CHOOSER_REQUEST_CODE = companionChooserRequestCode;

        this.manager = (BluetoothManager)activity.getSystemService(Context.BLUETOOTH_SERVICE);
        this.adapter = this.manager.getAdapter();

        IntentFilter filter = new IntentFilter(BluetoothDevice.ACTION_BOND_STATE_CHANGED);
        this.activity.get().registerReceiver(mReceiver, filter);

        Log.d("BleSession", "Constructed BleSession (Handle = " + sessionHandle + ")");
    }

    private final BroadcastReceiver mReceiver = new BroadcastReceiver()
    {
        @Override
        public void onReceive(Context context, Intent intent)
        {
            final String action = intent.getAction();
            if (action == null)
                return;

            if (sessionHandle == 0) {
                Log.d("BleSession", "Ignoring device bonding state change without session handle");
                return;
            }

            final BluetoothDevice device = intent.getParcelableExtra(BluetoothDevice.EXTRA_DEVICE);
            Long deviceHandle = deviceHandles.get(device);
            if (deviceHandle == null) {
                Log.d("BleSession", "Ignoring device bonding state change without device handle");
                return;
            }

            if (action.equals(BluetoothDevice.ACTION_BOND_STATE_CHANGED))
            {
                final int state = intent.getIntExtra(BluetoothDevice.EXTRA_BOND_STATE, BluetoothDevice.ERROR);
                final int prevState = intent.getIntExtra(BluetoothDevice.EXTRA_PREVIOUS_BOND_STATE, -1);

                onDeviceBondingStateChange(sessionHandle, deviceHandle, prevState, state);
                /*
                switch(state){
                    case BluetoothDevice.BOND_BONDING:
                        onDeviceBondingStateChange(sessionHandle, deviceHandle, prevState, state);
                        break;

                    case BluetoothDevice.BOND_BONDED:


                        //this.activity.unregisterReceiver(mReceiver);
                        break;

                    case BluetoothDevice.BOND_NONE:

                        break;
                }
                */
            }
        }
    };

    // Since the native session is constructed after this BleSession the handle is
    // associated after construction...
    public void setNativeSessionHandle(long sessionHandle) {
        this.sessionHandle = sessionHandle;

        // If necessary we could effectively consider this like a destructor,
        // since we're being explicitly disconnected from the native session
        if (sessionHandle == 0) {

        }
    }
    public long getNativeSessionHandle() {
        return this.sessionHandle;
    }

    public Context context() {
        return this.activity.get();
    }

    public BleDevice getDeviceForAddress(String address, long nativeHandle) {
        BluetoothDevice device = this.adapter.getRemoteDevice(address);
        return new BleDevice(device, nativeHandle);
    }

    //@RequiresApi(api = Build.VERSION_CODES.O)
    private void startScanViaCompanionAPI() {
        CompanionDeviceManager deviceManager =
                (CompanionDeviceManager) this.activity.get().getSystemService(
                        Context.COMPANION_DEVICE_SERVICE
                );
        Log.d("BleSession", "found deviceManager");

        // To skip filtering based on name and supported feature flags,
        // don't include calls to setNamePattern() and addServiceUuid(),
        // respectively. This example uses Bluetooth.
        BluetoothDeviceFilter deviceFilter =
                new BluetoothDeviceFilter.Builder()
                        //.setNamePattern(Pattern.compile("My device"))

                        // XXX: we can't use the addServiceUuid API with Android 9 since it will lead to
                        // null pointer exceptions in the companion device manager:
                        // https://code.yawk.at/android/android-9.0.0_r35/android/companion/BluetoothDeviceFilterUtils.java:84

                        //.addServiceUuid(
                        //        new ParcelUuid(new UUID(0x0000180d, 0x0)),
                        //        new ParcelUuid(new UUID(0x0000ffff, 0x0)) // Mask
                        //)

                        .build();
        Log.d("BleSession", "built device filter");

        // The argument provided in setSingleDevice() determines whether a single
        // device name or a list of device names is presented to the user as
        // pairing options.
        AssociationRequest pairingRequest = new AssociationRequest.Builder()
                .addDeviceFilter(deviceFilter)
                .setSingleDevice(false)
                .build();
        Log.d("BleSession", "built association request");

        Log.d("BleSession", "Calling deviceManager.associate()...");
        // When the app tries to pair with the Bluetooth device, show the
        // appropriate pairing request dialog to the user.
        deviceManager.associate(pairingRequest,
                new CompanionDeviceManager.Callback() {
                    @Override
                    public void onDeviceFound(IntentSender chooserLauncher) {
                        try {
                            Log.d("BleSession", "Sending chooser intent");
                            activity.get().startIntentSenderForResult(chooserLauncher,
                                    COMPANION_CHOOSER_REQUEST_CODE, null, 0, 0, 0);
                        } catch (IntentSender.SendIntentException e) {
                            Log.d("BleSession", "Failed to send chooser intent");
                            // failed to send the intent
                        }
                    }

                    @Override
                    public void onFailure(CharSequence error) {
                        Log.d("BleSession", "Failed to find companion device");
                        // handle failure to find the companion device
                    }
                }, null);
    }

    public static void onCompanionChooserResult(int resultCode, Intent data) {
        BluetoothDevice device = data.getParcelableExtra(
                CompanionDeviceManager.EXTRA_DEVICE
        );

        if (device != null) {
            Log.d("BleSession", "BLE: Found device!");
            if (BleSession.scannerSession != null) {
                BleSession session = BleSession.scannerSession;
                long nativeSession = session.getNativeSessionHandle();
                String address = device.getAddress();
                @SuppressLint("MissingPermission") String name = device.getName();
                session.onCompanionDeviceSelect(nativeSession, device, address, name);
            } else {
                Log.d("BleSession", "BLE: Got companion result with no active scanner session");
            }

            //deviceToPair.createBond();
            // ... Continue interacting with the paired device.
        }
    }

    private ScanCallback scannerCallback = null;
    private ScanSettings.Builder scannerSettingsBuilder = null;
    private List<ScanFilter> scannerFilters = new ArrayList<>();

    @SuppressLint("MissingPermission")
    private void startScanDirect() {
        scanner = adapter.getBluetoothLeScanner();

        BleSession session = this;

        scannerCallback = new ScanCallback() {
            @Override
            public void onScanResult(int callbackType, ScanResult result) {
                BluetoothDevice device = result.getDevice();
                String address = device.getAddress();
                session.onScanResult(sessionHandle, callbackType, result, address);
            }
        };

        //List<ScanFilter> filters = new ArrayList<>();
        //ScanFilter filter = new ScanFilter.Builder()
        //        .build();
        //filters.add(filter);
        // TODO: support building filters


        //ScanSettings settings = new ScanSettings.Builder()
        //        .setCallbackType(ScanSettings.CALLBACK_TYPE_ALL_MATCHES)
        //        .build();
        scanner.startScan(scannerFilters, scannerSettingsBuilder.build(), scannerCallback);
    }

    public void scannerConfigReset() throws Exception {
        if (BleSession.scannerSession != null) {
            throw new Exception("Can't change scanner config while scanning");
        }
        scannerSettingsBuilder = new ScanSettings.Builder();
        scannerFilters = new ArrayList<>();
    }

    public void scannerConfigAddServiceUuid(String uuidStr) {
        UUID serviceUuid = UUID.fromString(uuidStr);
        ScanFilter filter = new ScanFilter.Builder()
                .setServiceUuid(new ParcelUuid(serviceUuid))
                .build();
        scannerFilters.add(filter);
    }

    // Note: It's expected that scannerConfigReset() is called and any filter
    // settings are specified before calling startScanning()
    public void startScanning() throws Exception {
        Log.d("BleSession", "startScanning");
        if (Looper.getMainLooper().isCurrentThread()) {
            Log.d("BleSession", "scanning on main UI thread");
        } else {
            Log.d("BleSession", "scanning not on main UI thread");
        }

        if (BleSession.scannerSession != null) {
            throw new Exception("Can't scan from multiple bluetooth sessions in parallel");
        }

        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O && COMPANION_CHOOSER_REQUEST_CODE >= 0) {
            startScanViaCompanionAPI();
        } else {
            startScanDirect();
        }

        BleSession.scannerSession = this;
    }

    @SuppressLint("MissingPermission")
    public void stopScanning() throws Exception {
        if (BleSession.scannerSession != this) {
            throw new Exception("This BleSession isn't scanning (can't stopScanning)");
        }

        if (scanner != null) {
            scanner.stopScan(scannerCallback);
        }
        BleSession.scannerSession = null;
    }

    public int scanResultGetSpecificManufacturerDataCount(ScanResult result) {
        return result.getScanRecord().getManufacturerSpecificData().size();
    }

    public int scanResultGetSpecificManufacturerDataId(ScanResult result, int index) {
        return result.getScanRecord().getManufacturerSpecificData().keyAt(index);
    }

    public byte[] scanResultGetSpecificManufacturerData(ScanResult result, int manufacturerId) {
        return result.getScanRecord().getManufacturerSpecificData(manufacturerId);
    }

    public int scanResultGetServiceUuidsCount(ScanResult result) {
        List<ParcelUuid> uuids = result.getScanRecord().getServiceUuids();
        if (uuids != null) {
            return uuids.size();
        } else {
            return 0;
        }
    }

    public String scanResultGetNthServiceUuid(ScanResult result, int index) {
        List<ParcelUuid> resultUuids = result.getScanRecord().getServiceUuids();
        return resultUuids.get(index).toString();
    }

    //@RequiresApi(api = Build.VERSION_CODES.O)
    public int scanResultGetTxPowerLevel(ScanResult result) {
        return result.getTxPower();
    }

    public int scanResultGetRssi(ScanResult result) {
        return result.getRssi();
    }

    public String scanResultGetLocalName(ScanResult result) {
        return result.getScanRecord().getDeviceName();
    }

    // With some versions of Android then connecting with autoconnect=true
    // apparently only works with devices that are cached, so we have to
    // fallback to manually retrying to connect with autoconnect=false
    // for uncached devices.
    @SuppressLint("MissingPermission")
    public boolean isDeviceCached(BleDevice device) {
        return device.device.getType() != BluetoothDevice.DEVICE_TYPE_UNKNOWN;
    }

    @SuppressLint("MissingPermission")
    public void connectDeviceGatt(BleDevice device, long deviceHandle, boolean autoconnect) {
        BleSession session = this;

        device.connectedGatt = device.device.connectGatt(this.activity.get(),
                autoconnect,
                new BluetoothGattCallback() {
                    @Override
                    public void onConnectionStateChange(BluetoothGatt gatt,
                                                        int status,
                                                        int newState) {
                        boolean connected;
                        if (newState == BluetoothProfile.STATE_CONNECTED)
                            connected = true;
                        else
                            connected = false;

                        onDeviceConnectionStateChange(session.sessionHandle, deviceHandle, connected, status);
                    }

                    @Override
                    public void onServicesDiscovered(BluetoothGatt gatt,
                                                     int status) {
                        onDeviceServicesDiscovered(session.sessionHandle, deviceHandle, status);
                    }

                    @Override
                    public void onReadRemoteRssi(BluetoothGatt gatt,
                                                 int rssi,
                                                 int status) {
                        onDeviceReadRemoteRssi(session.sessionHandle, deviceHandle, rssi, status);
                    }

                    @Override
                    public void onCharacteristicChanged(BluetoothGatt gatt, BluetoothGattCharacteristic characteristic) {
                        Log.d("BleSession", "onCharacteristicChanged");
                        int serviceInstanceId = characteristic.getService().getInstanceId();
                        int characteristicInstanceId = characteristic.getInstanceId();
                        byte[] value = characteristic.getValue();

                        session.onCharacteristicChanged(
                                session.sessionHandle,
                                deviceHandle,
                                serviceInstanceId,
                                characteristicInstanceId,
                                value);
                    }

                    @Override
                    public void onCharacteristicRead(BluetoothGatt gatt,
                                                     BluetoothGattCharacteristic characteristic,
                                                     int status)
                    {
                        int instanceId = characteristic.getInstanceId();
                        byte[] value = characteristic.getValue();
                        session.onCharacteristicRead(session.sessionHandle, deviceHandle, instanceId, value, status);
                    }

                    @Override
                    public void onCharacteristicWrite(BluetoothGatt gatt,
                                                      BluetoothGattCharacteristic characteristic,
                                                      int status)
                    {
                        int instanceId = characteristic.getInstanceId();
                        session.onCharacteristicWrite(session.sessionHandle, deviceHandle, instanceId, status);
                    }

                    @Override
                    public void onDescriptorRead(BluetoothGatt gatt,
                                                 BluetoothGattDescriptor descriptor,
                                                 int status)
                    {
                        int descriptorId = device.getDescriptorId(descriptor);
                        byte[] value = descriptor.getValue();
                        session.onDescriptorRead(session.sessionHandle, deviceHandle, descriptorId, value, status);
                    }

                    @Override
                    public void onDescriptorWrite(BluetoothGatt gatt,
                                                  BluetoothGattDescriptor descriptor,
                                                  int status)
                    {
                        int descriptorId = device.getDescriptorId(descriptor);
                        session.onDescriptorWrite(session.sessionHandle, deviceHandle, descriptorId, status);
                    }
                },
                BluetoothDevice.TRANSPORT_LE);
    }

    @SuppressLint("MissingPermission")
    public boolean reconnectDeviceGatt(BleDevice device) {
        return device.connectedGatt.connect();
    }

    @SuppressLint("MissingPermission")
    public void disconnectDeviceGatt(BleDevice device) {
        device.connectedGatt.disconnect();
    }

    @SuppressLint("MissingPermission")
    public void closeDeviceGatt(BleDevice device) {
        device.connectedGatt.close();
        device.connectedGatt = null;
    }

    @SuppressLint("MissingPermission")
    public int getDeviceBondState(BleDevice device) {
        return device.device.getBondState();
    }

    @SuppressLint("MissingPermission")
    public String getDeviceName(BleDevice device) {
        return device.device.getName();
    }

    @SuppressLint("MissingPermission")
    public boolean readDeviceRssi(BleDevice device) {
        return device.connectedGatt.readRemoteRssi();
    }

    @SuppressLint("MissingPermission")
    public boolean discoverDeviceServices(BleDevice device) {
        return device.connectedGatt.discoverServices();
    }

    public List<BluetoothGattService> getDeviceServices(BleDevice device) {
        return device.connectedGatt.getServices();
    }

    public int getServiceInstanceId(BluetoothGattService service) {
        return service.getInstanceId();
    }

    public String getServiceUuid(BluetoothGattService service) {
        return service.getUuid().toString();
    }

    public List<BluetoothGattService> getServiceIncludes(BluetoothGattService service) {
        return service.getIncludedServices();
    }

    public List<BluetoothGattCharacteristic> getServiceCharacteristics(BluetoothGattService service) {
        return service.getCharacteristics();
    }

    @SuppressLint("MissingPermission")
    public boolean requestReadCharacteristic(BleDevice device, BluetoothGattCharacteristic characteristic) {
        return device.connectedGatt.readCharacteristic(characteristic);
    }

    @SuppressLint("MissingPermission")
    public boolean requestWriteCharacteristic(BleDevice device, BluetoothGattCharacteristic characteristic, byte[] value, int writeType) {
        characteristic.setWriteType(writeType);
        characteristic.setValue(value);
        return device.connectedGatt.writeCharacteristic(characteristic);
    }

    private static final UUID CCCD_UUID = UUID.fromString("00002902-0000-1000-8000-00805f9b34fb");
    @SuppressLint("MissingPermission")
    public boolean requestSubscribeCharacteristic(BleDevice device, BluetoothGattCharacteristic characteristic) {
        Log.d("BleSession", "requestSubscribeCharacteristic");
        BluetoothGattDescriptor cccd = characteristic.getDescriptor(CCCD_UUID);
        if (cccd != null) {
            Log.d("BleSession", "found cccd to configure notification");
            boolean status0 = device.connectedGatt.setCharacteristicNotification(characteristic, true);
            boolean status1 = cccd.setValue(BluetoothGattDescriptor.ENABLE_NOTIFICATION_VALUE);
            boolean status2 = device.connectedGatt.writeDescriptor(cccd);
            Log.d("BleSession", "status0 " + status0 + ", status1 = " + status1 + ", status2 = " + status2);
            return status0 && status1;
        } else {
            return false;
        }
    }

    @SuppressLint("MissingPermission")
    public boolean requestUnsubscribeCharacteristic(BleDevice device, BluetoothGattCharacteristic characteristic) {
        Log.d("BleSession", "requestUnsubscribeCharacteristic");
        BluetoothGattDescriptor cccd = characteristic.getDescriptor(CCCD_UUID);
        if (cccd != null) {
            Log.d("BleSession", "found cccd to configure notification");
            return cccd.setValue(BluetoothGattDescriptor.DISABLE_NOTIFICATION_VALUE) &&
                    device.connectedGatt.writeDescriptor(cccd) &&
                    device.connectedGatt.setCharacteristicNotification(characteristic, false);
        } else {
            return false;
        }
    }

    public int getCharacteristicInstanceId(BluetoothGattCharacteristic characteristic) {
        return characteristic.getInstanceId();
    }

    public String getCharacteristicUuid(BluetoothGattCharacteristic characteristic) {
        return characteristic.getUuid().toString();
    }

    public int getCharacteristicProperties(BluetoothGattCharacteristic characteristic) {
        return characteristic.getProperties();
    }

    public List<BluetoothGattDescriptor> getCharacteristicDescriptors(BluetoothGattCharacteristic characteristic) {
        return characteristic.getDescriptors();
    }

    public synchronized int getDescriptorId(BleDevice device, BluetoothGattDescriptor descriptor) {
        return device.getDescriptorId(descriptor);
    }

    public String getDescriptorUuid(BluetoothGattDescriptor descriptor) {
        return descriptor.getUuid().toString();
    }

    @SuppressLint("MissingPermission")
    public boolean requestReadDescriptor(BleDevice device, BluetoothGattDescriptor descriptor) {
        return device.connectedGatt.readDescriptor(descriptor);
    }

    @SuppressLint("MissingPermission")
    public boolean requestWriteDescriptor(BleDevice device, BluetoothGattDescriptor descriptor, byte[] value) {
        descriptor.setValue(value);
        return device.connectedGatt.writeDescriptor(descriptor);
    }

    // After disconnecting from a device we will invalidate all gatt service,
    // characteristic and descriptor state. Since we maintain descriptor
    // IDs in Java we need to clear those...
    public void clearDeviceGattState(BleDevice device) {
        device.clearGattState();
    }

    private native void onCompanionDeviceSelect(long sessionHandle, BluetoothDevice device, String address, String name);
    private native void onScanResult(long sessionHandle, int callbackType, ScanResult result, String address);
    private native void onDeviceConnectionStateChange(long sessionHandle, long deviceHandle, boolean connected, int status);
    private native void onDeviceBondingStateChange(long sessionHandle, long deviceHandle, int prevState, int newState);
    private native void onDeviceServicesDiscovered(long sessionHandle, long deviceHandle, int status);
    private native void onDeviceReadRemoteRssi(long sessionHandle, long deviceHandle, int rssi, int status);
    private native void onCharacteristicRead(long sessionHandle, long deviceHandle, int characteristicInstanceId, byte[] value, int status);
    private native void onCharacteristicWrite(long sessionHandle, long deviceHandle, int characteristicInstanceId, int status);
    private native void onCharacteristicChanged(long sessionHandle, long deviceHandle, int serviceInstanceId, int characteristicInstanceId, byte[] value);
    private native void onDescriptorRead(long sessionHandle, long deviceHandle, int descriptorId, byte[] value, int status);
    private native void onDescriptorWrite(long sessionHandle, long deviceHandle, int descriptorId, int status);
}
