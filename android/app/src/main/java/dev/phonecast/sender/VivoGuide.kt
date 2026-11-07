package dev.phonecast.sender

import android.content.ComponentName
import android.content.Context
import android.content.Intent
import android.net.Uri
import android.os.PowerManager
import android.provider.Settings
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.verticalScroll
import androidx.compose.material3.Button
import androidx.compose.material3.Card
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedButton
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Modifier
import androidx.compose.ui.unit.dp

/**
 * Guided screen for surviving aggressive OEM background management
 * (iQOO/vivo, Funtouch/Origin OS). Per the project rule, anything not
 * verifiable without the physical device is labelled as such in the UI
 * text, with dontkillmyapp.com/vivo referenced for manual fallback.
 *
 * Each row tries a deep-link first, falls back to the generic app-info
 * screen, and reports which one opened — so on the real device we learn
 * what actually works there instead of guessing silently.
 */
@Composable
fun VivoGuide(onBack: () -> Unit) {
    val context = androidx.compose.ui.platform.LocalContext.current
    var lastResult by remember { mutableStateOf<String?>(null) }

    Column(
        Modifier.fillMaxWidth().verticalScroll(rememberScrollState()).padding(16.dp),
        verticalArrangement = Arrangement.spacedBy(10.dp),
    ) {
        Text("Stay alive while streaming (vivo/iQOO)", style = MaterialTheme.typography.headlineSmall)
        Text(
            "Android's foreground service already protects the stream; these OEM-specific " +
                "settings close the rest of the gap. Steps marked ⚠ can only be verified on " +
                "your actual phone — tell me which ones opened the right screen.",
            style = MaterialTheme.typography.bodySmall,
        )

        GuideRow(
            "Ignore battery optimizations (standard Android API)",
            "Exempts PhoneCast from Doze-style killing. This one is a documented API.",
        ) {
            val pm = context.getSystemService(Context.POWER_SERVICE) as PowerManager
            if (pm.isIgnoringBatteryOptimizations(context.packageName)) {
                "Already exempted ✔"
            } else {
                try {
                    context.startActivity(
                        Intent(Settings.ACTION_REQUEST_IGNORE_BATTERY_OPTIMIZATIONS)
                            .setData(Uri.parse("package:${context.packageName}")),
                    )
                    "Opened the system exemption dialog — press Allow."
                } catch (_: Exception) {
                    openAppInfo(context)
                    "System dialog unavailable; opened app info instead."
                }
            }
        }

        GuideRow(
            "⚠ Autostart management (vivo)",
            "Keeps the app runnable after swipe-kill/reboot. Known vivo component names tried in order.",
        ) {
            val candidates = listOf(
                ComponentName("com.vivo.permissionmanager", "com.vivo.permissionmanager.activity.BgStartUpManagerActivity"),
                ComponentName("com.iqoo.secure", "com.iqoo.secure.ui.phoneoptimize.BgStartUpManager"),
                ComponentName("com.iqoo.secure", "com.iqoo.secure.MainActivity"),
            )
            val opened = candidates.firstOrNull { runCatching { context.startActivity(Intent().setComponent(it)) }.isSuccess }
            if (opened != null) "Opened ${opened.packageName} — find PhoneCast and allow autostart."
            else { openAppInfo(context); "No known vivo settings screen opened; opened app info. Do it manually per dontkillmyapp.com/vivo." }
        }

        GuideRow(
            "⚠ Background power consumption (vivo)",
            "Set PhoneCast to 'High background power consumption' / unrestricted.",
        ) {
            val candidates = listOf(
                ComponentName("com.vivo.settings", "com.vivo.settings.AppPowerUsageDetails"),
                ComponentName("com.iqoo.secure", "com.iqoo.secure.ui.appoptimize.PowerWhitelistActivity"),
            )
            val opened = candidates.firstOrNull { runCatching { context.startActivity(Intent().setComponent(it)) }.isSuccess }
            if (opened != null) "Opened ${opened.packageName} — set PhoneCast unrestricted."
            else { openAppInfo(context); "Deep link unavailable; opened app info — search for Battery > PhoneCast." }
        }

        Card(Modifier.fillMaxWidth()) {
            Column(Modifier.padding(12.dp), verticalArrangement = Arrangement.spacedBy(4.dp)) {
                Text("⚠ Manual steps (no API exists for these)")
                Text("1. Recent apps: long-press PhoneCast → Lock (or pull down → lock).")
                Text("2. i Manager / Battery Manager: disable 'Smart keep background' restrictions for this app.")
                Text("3. Reference checklist: dontkillmyapp.com/vivo — we could not verify which items this OS build actually has.")
            }
        }

        lastResult?.let { Text(it, color = MaterialTheme.colorScheme.primary) }

        Button(onClick = { onBack() }, modifier = Modifier.fillMaxWidth()) {
            Text("Back / done")
        }
    }
}

@Composable
private fun GuideRow(title: String, detail: String, action: () -> String) {
    var result by remember { mutableStateOf<String?>(null) }
    Column(Modifier.fillMaxWidth()) {
        Text(title, style = MaterialTheme.typography.titleSmall)
        Text(detail, style = MaterialTheme.typography.bodySmall)
        Row(Modifier.padding(top = 4.dp)) {
            OutlinedButton(onClick = { result = action() }) { Text("Open") }
            result?.let {
                Text("  → $it", style = MaterialTheme.typography.bodySmall)
            }
        }
    }
}

private fun openAppInfo(context: Context) {
    runCatching {
        context.startActivity(
            Intent(Settings.ACTION_APPLICATION_DETAILS_SETTINGS)
                .setData(Uri.parse("package:${context.packageName}")),
        )
    }
}
