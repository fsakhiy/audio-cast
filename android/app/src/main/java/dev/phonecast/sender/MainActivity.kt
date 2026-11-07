package dev.phonecast.sender

import android.Manifest
import android.content.Context
import android.content.Intent
import android.media.projection.MediaProjectionManager
import android.os.Build
import android.os.Bundle
import androidx.activity.ComponentActivity
import androidx.activity.compose.rememberLauncherForActivityResult
import androidx.activity.compose.setContent
import androidx.activity.enableEdgeToEdge
import androidx.activity.result.contract.ActivityResultContracts
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.imePadding
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.text.KeyboardOptions
import androidx.compose.foundation.verticalScroll
import androidx.compose.material3.Button
import androidx.compose.material3.Card
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedButton
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.Scaffold
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.saveable.rememberSaveable
import androidx.compose.runtime.setValue
import androidx.compose.ui.Modifier
import androidx.compose.ui.unit.dp
import androidx.core.content.ContextCompat
import androidx.lifecycle.compose.collectAsStateWithLifecycle
import kotlinx.coroutines.flow.update

class MainActivity : ComponentActivity() {

    private lateinit var projectionManager: MediaProjectionManager

    // Fresh consent for every session: the token from a previous dialog is
    // never reused (Android 14+ requires it, and it's just correct).
    private val consentLauncher =
        registerForActivityResult(ActivityResultContracts.StartActivityForResult()) { result ->
            if (result.resultCode == RESULT_OK && result.data != null) {
                startStreaming(result.resultCode, result.data!!)
            } else {
                StreamState.ui.update { it.copy(statusLine = "Cancelled — screen-capture consent is required") }
            }
        }

    private val notificationsLauncher =
        registerForActivityResult(ActivityResultContracts.RequestPermission()) { /* denial is non-fatal */ }

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        enableEdgeToEdge()
        projectionManager = getSystemService(MEDIA_PROJECTION_SERVICE) as MediaProjectionManager
        if (Build.VERSION.SDK_INT >= 33) {
            notificationsLauncher.launch(Manifest.permission.POST_NOTIFICATIONS)
        }
        setContent {
            MaterialTheme { PhoneCastScreen(applicationContext) }
        }
    }

    private fun startStreaming(resultCode: Int, resultData: Intent) {
        val intent = Intent(this, StreamService::class.java).apply {
            action = StreamService.ACTION_START
            putExtra(StreamService.EXTRA_HOST, settings.host)
            putExtra(StreamService.EXTRA_TCP_PORT, settings.tcpPort.toIntOrNull() ?: 53000)
            putExtra(StreamService.EXTRA_UDP_PORT, settings.udpPort.toIntOrNull() ?: 53001)
            putExtra(StreamService.EXTRA_TOKEN, settings.token)
            putExtra(StreamService.EXTRA_RESULT_CODE, resultCode)
            putExtra(StreamService.EXTRA_RESULT_DATA, resultData)
        }
        ContextCompat.startForegroundService(this, intent)
    }

    // Held by the Activity so entries survive rotation; a real config store
    // (persisted across launches) arrives with milestone 5.
    internal val settings = SenderSettings()

    companion object {
        fun stop(context: Context) {
            ContextCompat.startForegroundService(
                context,
                Intent(context, StreamService::class.java).apply { action = StreamService.ACTION_STOP },
            )
        }

        fun requestConsent(activity: MainActivity) {
            activity.consentLauncher.launch(activity.projectionManager.createScreenCaptureIntent())
        }
    }
}

class SenderSettings {
    var host by mutableStateOf("")
    var tcpPort by mutableStateOf("53000")
    var udpPort by mutableStateOf("53001")
    var token by mutableStateOf("")
}

@Composable
fun PhoneCastScreen(context: Context) {
    val activity = context as MainActivity
    val state = activity.settings
    val ui by StreamState.ui.collectAsStateWithLifecycle()

    var error by rememberSaveable { mutableStateOf<String?>(null) }

    Scaffold { padding ->
        Column(
            modifier = Modifier
                .padding(padding)
                .padding(16.dp)
                .fillMaxWidth()
                .verticalScroll(rememberScrollState())
                .imePadding(),
            verticalArrangement = Arrangement.spacedBy(12.dp),
        ) {
            Text("PhoneCast sender", style = MaterialTheme.typography.headlineSmall)

            OutlinedTextField(
                value = state.host,
                onValueChange = { state.host = it },
                label = { Text("Receiver address (e.g. 192.168.1.20)") },
                keyboardOptions = KeyboardOptions(autoCorrect = false),
                singleLine = true,
                enabled = !ui.running,
                modifier = Modifier.fillMaxWidth(),
            )
            Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
                OutlinedTextField(
                    value = state.tcpPort,
                    onValueChange = { state.tcpPort = it.filter(Char::isDigit) },
                    label = { Text("TCP") },
                    singleLine = true,
                    enabled = !ui.running,
                    modifier = Modifier.weight(1f),
                )
                OutlinedTextField(
                    value = state.udpPort,
                    onValueChange = { state.udpPort = it.filter(Char::isDigit) },
                    label = { Text("UDP") },
                    singleLine = true,
                    enabled = !ui.running,
                    modifier = Modifier.weight(1f),
                )
            }
            OutlinedTextField(
                value = state.token,
                onValueChange = { state.token = it },
                label = { Text("Pairing token (optional until milestone 5)") },
                singleLine = true,
                enabled = !ui.running,
                modifier = Modifier.fillMaxWidth(),
            )

            Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
                Button(
                    enabled = !ui.running,
                    onClick = {
                        error = validate(state)
                        if (error == null) MainActivity.requestConsent(activity)
                    },
                ) { Text("Start") }
                OutlinedButton(
                    enabled = ui.running,
                    onClick = { MainActivity.stop(context) },
                ) { Text("Stop") }
            }

            error?.let { Text(it, color = MaterialTheme.colorScheme.error) }

            Card(Modifier.fillMaxWidth()) {
                Column(Modifier.padding(12.dp), verticalArrangement = Arrangement.spacedBy(4.dp)) {
                    Text(ui.statusLine)
                    Text("Datagrams sent: ${ui.packetsSent}   Bytes: ${ui.bytesSent / 1024} KiB")
                    if (ui.captureQuiet) {
                        Text(
                            "No audio captured for 3+ s. The playing app may block " +
                                "playback capture (FLAG_SECURE/opt-out) or its audio " +
                                "usage isn't MEDIA/GAME.",
                            color = MaterialTheme.colorScheme.tertiary,
                        )
                    }
                }
            }

            Text(
                "Play any app's music after pressing Start — it should come out of the PC's " +
                    "output device. System notifications and calls are not captured.",
                style = MaterialTheme.typography.bodySmall,
            )
        }
    }
}

private fun validate(s: SenderSettings): String? {
    if (s.host.isBlank()) return "Enter the receiver's LAN address first."
    if (s.host.any { it.isWhitespace() }) return "Host field takes an IP or hostname only."
    val tcp = s.tcpPort.toIntOrNull() ?: return "TCP port must be a number."
    val udp = s.udpPort.toIntOrNull() ?: return "UDP port must be a number."
    if (tcp !in 1..65535 || udp !in 1..65535) return "Ports must be 1–65535."
    if (s.token.toByteArray(Charsets.US_ASCII).size > 32) return "Token must be ≤ 32 ASCII bytes."
    return null
}
