package app.sanctuaryplayer.android;

import android.content.BroadcastReceiver;
import android.content.Context;
import android.content.Intent;
import android.content.IntentFilter;
import android.media.AudioAttributes;
import android.media.AudioFocusRequest;
import android.media.AudioManager;
import android.os.Build;
import android.os.Bundle;

import com.google.androidgamesdk.GameActivity;

/**
 * Android host for the Rust SanctuaryPlayer application.
 *
 * GameActivity supplies GameTextInput and Android lifecycle integration;
 * playback, UI and rendering remain in Rust. This class owns the small amount
 * of Java-side Android media integration that requires framework listeners.
 */
public final class SanctuaryPlayerActivity extends GameActivity {
    private AudioManager audioManager;
    private AudioFocusRequest audioFocusRequest;
    private AudioManager.OnAudioFocusChangeListener audioFocusChangeListener;
    private BroadcastReceiver becomingNoisyReceiver;
    private boolean becomingNoisyReceiverRegistered;
    private boolean mediaIntegrationInitialised;
    private String pendingDeepLink;

    private static native void nativeOnAudioFocusChange(int focusChange);
    private static native void nativeOnBecomingNoisy();
    private static native void nativeOnDeepLinkAvailable();

    @Override
    protected void onCreate(Bundle savedInstanceState) {
        Intent intent = getIntent();
        if (intent != null && intent.getData() != null) {
            pendingDeepLink = intent.getDataString();
        }
        super.onCreate(savedInstanceState);
    }

    @Override
    protected void onNewIntent(Intent intent) {
        super.onNewIntent(intent);
        setIntent(intent);
        pendingDeepLink = intent != null ? intent.getDataString() : null;
        if (pendingDeepLink != null) {
            nativeOnDeepLinkAvailable();
        }
    }

    /** Return and clear the most recent deep link delivered to this Activity. */
    public String takePendingDeepLink() {
        String deepLink = pendingDeepLink;
        pendingDeepLink = null;
        return deepLink;
    }

    /** Initialise audio-focus and output-routing listeners. Safe to call repeatedly. */
    public void initialiseMediaIntegration() {
        if (mediaIntegrationInitialised) {
            return;
        }

        audioManager = (AudioManager) getSystemService(Context.AUDIO_SERVICE);
        audioFocusChangeListener = SanctuaryPlayerActivity::nativeOnAudioFocusChange;

        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) {
            AudioAttributes attributes = new AudioAttributes.Builder()
                    .setUsage(AudioAttributes.USAGE_MEDIA)
                    .setContentType(AudioAttributes.CONTENT_TYPE_MOVIE)
                    .build();
            audioFocusRequest = new AudioFocusRequest.Builder(AudioManager.AUDIOFOCUS_GAIN)
                    .setAudioAttributes(attributes)
                    .setAcceptsDelayedFocusGain(false)
                    .setWillPauseWhenDucked(true)
                    .setOnAudioFocusChangeListener(audioFocusChangeListener)
                    .build();
        }

        becomingNoisyReceiver = new BroadcastReceiver() {
            @Override
            public void onReceive(Context context, Intent intent) {
                if (AudioManager.ACTION_AUDIO_BECOMING_NOISY.equals(intent.getAction())) {
                    nativeOnBecomingNoisy();
                }
            }
        };
        IntentFilter filter = new IntentFilter(AudioManager.ACTION_AUDIO_BECOMING_NOISY);
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU) {
            registerReceiver(becomingNoisyReceiver, filter, Context.RECEIVER_EXPORTED);
        } else {
            registerReceiver(becomingNoisyReceiver, filter);
        }
        becomingNoisyReceiverRegistered = true;
        mediaIntegrationInitialised = true;
    }

    /** Request full media audio focus. Delayed focus is deliberately not accepted. */
    public boolean requestPlaybackAudioFocus() {
        initialiseMediaIntegration();
        if (audioManager == null) {
            return false;
        }

        final int result;
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) {
            result = audioManager.requestAudioFocus(audioFocusRequest);
        } else {
            result = audioManager.requestAudioFocus(
                    audioFocusChangeListener,
                    AudioManager.STREAM_MUSIC,
                    AudioManager.AUDIOFOCUS_GAIN);
        }
        return result == AudioManager.AUDIOFOCUS_REQUEST_GRANTED;
    }

    /** Release a previously granted/requested media audio focus claim. */
    public void abandonPlaybackAudioFocus() {
        if (audioManager == null || audioFocusChangeListener == null) {
            return;
        }
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O && audioFocusRequest != null) {
            audioManager.abandonAudioFocusRequest(audioFocusRequest);
        } else {
            audioManager.abandonAudioFocus(audioFocusChangeListener);
        }
    }

    /** Keep the display awake only while foreground video playback requests it. */
    public void setPlaybackKeepsScreenOn(boolean keepScreenOn) {
        runOnUiThread(() -> {
            if (keepScreenOn) {
                getWindow().addFlags(android.view.WindowManager.LayoutParams.FLAG_KEEP_SCREEN_ON);
            } else {
                getWindow().clearFlags(android.view.WindowManager.LayoutParams.FLAG_KEEP_SCREEN_ON);
            }
        });
    }

    /** Tear down listeners before the native activity exits. */
    public void shutdownMediaIntegration() {
        abandonPlaybackAudioFocus();
        if (becomingNoisyReceiverRegistered && becomingNoisyReceiver != null) {
            unregisterReceiver(becomingNoisyReceiver);
            becomingNoisyReceiverRegistered = false;
        }
        becomingNoisyReceiver = null;
        audioFocusRequest = null;
        audioFocusChangeListener = null;
        audioManager = null;
        mediaIntegrationInitialised = false;
        setPlaybackKeepsScreenOn(false);
    }
}