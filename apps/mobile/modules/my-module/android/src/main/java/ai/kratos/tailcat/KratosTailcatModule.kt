package ai.kratos.tailcat

import android.util.Base64
import expo.modules.kotlin.modules.Module
import expo.modules.kotlin.modules.ModuleDefinition
import org.json.JSONObject
import tailcatnative.Client
import tailcatnative.Tailcatnative

class KratosTailcatModule : Module() {
  private var client: Client? = null

  override fun definition() = ModuleDefinition {
    Name("KratosTailcat")

    AsyncFunction("startProbe") { invitation: String ->
      client?.close()
      val payload = invitation.trim().removePrefix("kratos-pair:")
      val json = if (payload.startsWith('{')) payload else String(
        Base64.decode(payload, Base64.URL_SAFE or Base64.NO_PADDING or Base64.NO_WRAP)
      )
      val invite = JSONObject(json)
      require(invite.optInt("version") == 1) { "Unsupported invitation" }
      val address = invite.optString("address")
      require(address.isNotBlank()) { "Invitation has no Tailcat address" }
      val stateDirectory = requireNotNull(appContext.reactContext).filesDir.resolve("tailcat-probe")
      stateDirectory.mkdirs()
      val start = if (BuildConfig.DEBUG) Tailcatnative::startClientDiagnostic else Tailcatnative::startClient
      start(address, stateDirectory.absolutePath, invite.optString("derpMap")).also {
        client = it
      }.url()
    }

    Function("stop") {
      client?.close()
      client = null
    }

    OnDestroy {
      client?.close()
      client = null
    }
  }
}
