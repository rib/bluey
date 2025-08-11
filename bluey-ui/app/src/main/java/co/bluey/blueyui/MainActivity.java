package co.bluey.blueyui;

import androidx.annotation.NonNull;
import androidx.appcompat.app.AppCompatActivity;
import androidx.core.app.ActivityCompat;
import androidx.core.content.ContextCompat;
import androidx.core.view.WindowCompat;
import androidx.core.view.WindowInsetsCompat;
import androidx.core.view.WindowInsetsControllerCompat;

import com.google.androidgamesdk.GameActivity;

import android.Manifest;
import android.app.Activity;
import android.content.Intent;
import android.os.Bundle;
import android.content.pm.PackageManager;
import android.os.Build.VERSION;
import android.os.Build.VERSION_CODES;
import android.os.Bundle;
import android.view.View;
import android.view.WindowManager;
import android.util.Log;

import java.util.Objects;

import co.bluey.BleSession;

public class MainActivity extends GameActivity {
    private static final int BLUETOOTH_SCAN_CONNECT_PERMISSION_REQUEST_CODE = 1001;
    private static final int BLUETOOTH_CONNECT_PERMISSION_REQUEST_CODE = 1002;

    static {
        // Load the STL first to workaround issues on old Android versions:
        // "if your app targets a version of Android earlier than Android 4.3
        // (Android API level 18),
        // and you use libc++_shared.so, you must load the shared library before any other
        // library that depends on it."
        // See https://developer.android.com/ndk/guides/cpp-support#shared_runtimes
        //System.loadLibrary("c++_shared");

        // Load the native library.
        // The name "android-game" depends on your CMake configuration, must be
        // consistent here and inside AndroidManifect.xml
        System.loadLibrary("main");
    }

    // Check if BLUETOOTH_SCAN permission is granted (Android 12+)
    public boolean checkBluetoothScanPermission() {
        return ContextCompat.checkSelfPermission(this,
                Manifest.permission.BLUETOOTH_SCAN) == PackageManager.PERMISSION_GRANTED;
    }

    // Check if BLUETOOTH_CONNECT permission is granted (Android 12+)
    public boolean checkBluetoothConnectPermission() {
        return ContextCompat.checkSelfPermission(this,
                Manifest.permission.BLUETOOTH_CONNECT) == PackageManager.PERMISSION_GRANTED;
    }

    // Request BLUETOOTH_SCAN + BLUETOOTH_CONNECT permission from user
    public void requestBluetoothScanConnectPermission() {
        ActivityCompat.requestPermissions(this,
            new String[]{Manifest.permission.BLUETOOTH_SCAN, Manifest.permission.BLUETOOTH_CONNECT},
            BLUETOOTH_SCAN_CONNECT_PERMISSION_REQUEST_CODE);
    }

    // Request BLUETOOTH_CONNECT permission from user
    public void requestBluetoothConnectPermission() {
        ActivityCompat.requestPermissions(this,
                new String[]{Manifest.permission.BLUETOOTH_CONNECT},
                BLUETOOTH_CONNECT_PERMISSION_REQUEST_CODE);
    }

    private void hideSystemUI() {
        // This will put the game behind any cutouts and waterfalls on devices which have
        // them, so the corresponding insets will be non-zero.
        getWindow().getAttributes().layoutInDisplayCutoutMode
                = WindowManager.LayoutParams.LAYOUT_IN_DISPLAY_CUTOUT_MODE_ALWAYS;
        // From API 30 onwards, this is the recommended way to hide the system UI, rather than
        // using View.setSystemUiVisibility.
        View decorView = getWindow().getDecorView();
        WindowInsetsControllerCompat controller = new WindowInsetsControllerCompat(getWindow(),
                decorView);
        controller.hide(WindowInsetsCompat.Type.systemBars());
        controller.hide(WindowInsetsCompat.Type.displayCutout());
        controller.setSystemBarsBehavior(
                WindowInsetsControllerCompat.BEHAVIOR_SHOW_TRANSIENT_BARS_BY_SWIPE);
    }

    @Override
    protected void onCreate(Bundle savedInstanceState) {
        // When true, the app will fit inside any system UI windows.
        // When false, we render behind any system UI windows.
        WindowCompat.setDecorFitsSystemWindows(getWindow(), false);
        hideSystemUI();
        // You can set IME fields here or in native code using GameActivity_setImeEditorInfoFields.
        // We set the fields in native_engine.cpp.
        // super.setImeEditorInfoFields(InputType.TYPE_CLASS_TEXT,
        //     IME_ACTION_NONE, IME_FLAG_NO_FULLSCREEN );
        super.onCreate(savedInstanceState);
    }

    @Override
    protected void onActivityResult(int requestCode, int resultCode, Intent data) {
        if (requestCode == BleSession.COMPANION_CHOOSER_REQUEST_CODE) {
            if (resultCode == Activity.RESULT_OK && data != null) {
                BleSession.onCompanionChooserResult(resultCode, data);
            }
        } else {
            super.onActivityResult(requestCode, resultCode, data);
        }
    }

    @Override
    public void onRequestPermissionsResult(int requestCode, @NonNull String[] permissions, @NonNull int[] grantResults) {
        super.onRequestPermissionsResult(requestCode, permissions, grantResults);

        if (permissions.length != grantResults.length) {
            Log.e("MainActivity", "Spurious OnRequestPermissionResult with mismatching permissions / grantResults lengths");
            return;
        }
        if (requestCode == BLUETOOTH_SCAN_CONNECT_PERMISSION_REQUEST_CODE || requestCode == BLUETOOTH_CONNECT_PERMISSION_REQUEST_CODE) {
            boolean canConnect = false;
            boolean canScan = false;

            for (int i = 0; i < permissions.length; i++) {
                if (Objects.equals(permissions[i], Manifest.permission.BLUETOOTH_CONNECT)) {
                    if (grantResults[i] == PackageManager.PERMISSION_GRANTED) {
                        canConnect = true;
                    }
                } else if (Objects.equals(permissions[i], Manifest.permission.BLUETOOTH_SCAN)) {
                    if (grantResults[i] == PackageManager.PERMISSION_GRANTED) {
                        canScan = true;
                    }
                }
            }
            boolean status = false;
            if (requestCode == BLUETOOTH_SCAN_CONNECT_PERMISSION_REQUEST_CODE) {
                status = canConnect && canScan;
                Log.w("MainActivity", "BLUETOOTH_SCAN permission " + (canScan ? "granted" : "denied") + ", BLUETOOTH_CONNECT permission " + (canConnect ? "granted" : "denied"));
            } else if (requestCode == BLUETOOTH_CONNECT_PERMISSION_REQUEST_CODE) {
                status = canConnect;
                Log.w("MainActivity", "BLUETOOTH_CONNECT permission " + (canConnect ? "granted" : "denied"));
            }

            // Notify the native code about the permission result
            onBluetoothScanPermissionResult(status);
        }
    }

    // Native method to notify about Bluetooth scan permission result
    private native void onBluetoothScanPermissionResult(boolean granted);

    public boolean isGooglePlayGames() {
        PackageManager pm = getPackageManager();
        return pm.hasSystemFeature("com.google.android.play.feature.HPE_EXPERIENCE");
    }
}