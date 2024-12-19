package org.equalitie.ouisync.example

import android.util.Log
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.setValue
import androidx.lifecycle.ViewModel
import androidx.lifecycle.viewModelScope
import kotlinx.coroutines.launch
import org.equalitie.ouisync.lib.Repository
import org.equalitie.ouisync.lib.Session
import org.equalitie.ouisync.lib.ShareToken
import java.io.File

private val DB_EXTENSION = "ouisyncdb"
private const val TAG = "ouisync.example"

class ExampleViewModel(
    private val socketPath: String,
    private val configDir: String,
    private val storeDir: String,
) : ViewModel() {
    private var session: Session? = null

    var sessionError by mutableStateOf<String?>(null)
        private set

    var repositories by mutableStateOf<Map<String, Repository>>(mapOf())
        private set

    init {
        viewModelScope.launch {
            try {
                session = Session.create(socketPath, configDir)
            } catch (e: Exception) {
                Log.e(TAG, "Session.create failed", e)
                sessionError = e.toString()
            } catch (e: java.lang.Error) {
                Log.e(TAG, "Session.create failed", e)
                sessionError = e.toString()
            }

            session?.let {
                // Bind the network sockets to all interfaces and random ports. Use only the QUIC
                // protocol and use both IPv4 and IPv6.
                it.bindNetwork(quicV4 = "0.0.0.0:0", quicV6 = "[::]:0")

                // Enable port forwarding (UPnP) to improve chances of connecting to peers.
                it.setPortForwardingEnabled(true)

                // Enable Local Disocvery to automatically discover peers on the local network.
                it.setLocalDiscoveryEnabled(true)
            }

            openRepositories()
        }
    }

    suspend fun createRepository(name: String, token: String) {
        val session = this.session ?: return

        if (repositories.containsKey(name)) {
            Log.e(TAG, "repository named \"$name\" already exists")
            return
        }

        var shareToken: ShareToken? = null

        if (!token.isEmpty()) {
            shareToken = ShareToken.fromString(session, token)
        }

        val repo = Repository.create(
            session,
            "$storeDir/$name.$DB_EXTENSION",
            readSecret = null,
            writeSecret = null,
            shareToken = shareToken,
        )

        // Syncing is initially disabled, need to enable it.
        repo.setSyncEnabled(true)

        // Enable DHT and PEX for discovering peers. These settings are persisted so it's not
        // necessary to set them again when opening the repository later.
        repo.setDhtEnabled(true)
        repo.setPexEnabled(true)

        repositories = repositories + (name to repo)
    }

    suspend fun deleteRepository(name: String) {
        val repo = repositories.get(name) ?: return

        repositories = repositories - name

        repo.close()

        val baseName = "$name.$DB_EXTENSION"
        val files = File(storeDir).listFiles() ?: arrayOf()

        // A ouisync repository database consist of multiple files having the same prefix. Delete
        // all of them.
        for (file in files) {
            if (file.getName().startsWith(baseName)) {
                file.delete()
            }
        }
    }

    private suspend fun openRepositories() {
        val session = this.session ?: return
        val files = File(storeDir).listFiles() ?: arrayOf()

        for (file in files) {
            if (file.getName().endsWith(".$DB_EXTENSION")) {
                try {
                    val name = file
                        .getName()
                        .substring(0, file.getName().length - DB_EXTENSION.length - 1)
                    val repo = Repository.open(session, file.getPath())

                    // Syncing is initially disabled, enable it.
                    repo.setSyncEnabled(true)

                    // NOTE: The DHT and PEX settings are persisted from when the repo was created,
                    // so it's not necessary to set them again here.

                    Log.i(TAG, "Opened repository $name")

                    repositories = repositories + (name to repo)
                } catch (e: Exception) {
                    Log.e(TAG, "Failed to open repository at ${file.getPath()}")
                    continue
                }
            }
        }
    }

    override fun onCleared() {
        val repos = repositories.values
        repositories = mapOf()

        viewModelScope.launch {
            for (repo in repos) {
                repo.close()
            }

            session?.close()
            session = null
        }
    }
}
