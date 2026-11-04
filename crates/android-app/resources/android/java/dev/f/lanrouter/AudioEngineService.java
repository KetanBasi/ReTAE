package dev.f.lanrouter;

import android.app.Service;
import android.content.Intent;
import android.os.IBinder;

// * The foreground service that keeps the audio engine alive with the screen off.
// * Android needs a manifest-declared class in the APK's dex for this, so it is Java. It holds NO
// * logic: every callback forwards to a Rust `native` method registered in `JNI_OnLoad`
// * (`crates/android-app/src/natives.rs` lists them, and a test checks this file against that list).
public class AudioEngineService extends Service {
    // ? Makepad's activity loads the same library under this name. Loading it again is harmless
    // ? (the call is idempotent) and covers a start order where the service class loads first.
    static {
        System.loadLibrary("makepad");
    }

    @Override
    public void onCreate() {
        super.onCreate();
        nativeOnCreate();
    }

    @Override
    public int onStartCommand(Intent intent, int flags, int startId) {
        return nativeOnStartCommand(intent, startId);
    }

    @Override
    public void onDestroy() {
        nativeOnDestroy();
        super.onDestroy();
    }

    @Override
    public IBinder onBind(Intent intent) {
        return null;
    }

    private native void nativeOnCreate();

    private native int nativeOnStartCommand(Intent intent, int startId);

    private native void nativeOnDestroy();
}
