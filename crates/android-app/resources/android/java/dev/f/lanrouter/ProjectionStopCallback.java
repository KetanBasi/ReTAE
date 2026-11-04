package dev.f.lanrouter;

import android.media.projection.MediaProjection;

// * `MediaProjection.Callback` is an abstract class, so Rust cannot hand one to
// * `registerCallback` without a Java subclass. This is that subclass: one forward.
// * `onStop` also fires when the screen locks on Android 15 QPR1+ (PLAN.md §6.3 step 5).
public class ProjectionStopCallback extends MediaProjection.Callback {
    static {
        System.loadLibrary("makepad");
    }

    @Override
    public void onStop() {
        nativeOnStop();
    }

    private native void nativeOnStop();
}
