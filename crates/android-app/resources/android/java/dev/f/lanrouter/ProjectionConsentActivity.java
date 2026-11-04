package dev.f.lanrouter;

import android.app.Activity;
import android.content.Intent;
import android.os.Bundle;

// * A transparent Activity that asks for the two things only an Activity can ask for: the screen
// * capture consent (app audio) and the runtime permissions (microphone, notifications).
// * It exists so the app does not depend on whether Makepad's activity forwards
// * `onActivityResult`. No logic here: Rust starts the dialog, reads the answer and calls
// * `finish()` over JNI.
public class ProjectionConsentActivity extends Activity {
    static {
        System.loadLibrary("makepad");
    }

    @Override
    protected void onCreate(Bundle savedInstanceState) {
        super.onCreate(savedInstanceState);
        nativeOnCreate(getIntent());
    }

    @Override
    protected void onActivityResult(int requestCode, int resultCode, Intent data) {
        super.onActivityResult(requestCode, resultCode, data);
        nativeOnActivityResult(requestCode, resultCode, data);
    }

    @Override
    public void onRequestPermissionsResult(int requestCode, String[] permissions, int[] grantResults) {
        super.onRequestPermissionsResult(requestCode, permissions, grantResults);
        nativeOnRequestPermissionsResult(requestCode, permissions, grantResults);
    }

    @Override
    protected void onDestroy() {
        nativeOnDestroy();
        super.onDestroy();
    }

    private native void nativeOnCreate(Intent intent);

    private native void nativeOnActivityResult(int requestCode, int resultCode, Intent data);

    private native void nativeOnRequestPermissionsResult(
            int requestCode, String[] permissions, int[] grantResults);

    private native void nativeOnDestroy();
}
