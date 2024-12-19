package org.equalitie.ouisync.lib

import kotlinx.coroutines.test.runTest
import org.junit.After
import org.junit.Before
import org.junit.Test
import kotlin.io.path.createTempDirectory
import java.io.File as JFile

class SyncTest {
    lateinit var tempDir: JFile
    lateinit var sessionA: Session
    lateinit var sessionB: Session

    @Before
    fun setup() = runTest {
        tempDir = JFile(createTempDirectory().toString())

        sessionA = Session.create(
            socketPath = "$tempDir/a/sock",
            configPath = "$tempDir/a/config",
        )

        sessionB = Session.create(
            socketPath = "$tempDir/b/sock",
            configPath = "$tempDir/b/config",
        )
    }

    @After
    fun teardown() = runTest {
        sessionA.close()
        sessionB.close()
        tempDir.deleteRecursively()
    }

    @Test
    fun sync() = runTest {
        val repoA = Repository.create(
            sessionA,
            "$tempDir/a.ouisyncdb",
            readSecret = null,
            writeSecret = null,
        )

        val token = repoA.createShareToken()
        val repoB = Repository.create(
            sessionB,
            "$tempDir/b.ouisyncdb",
            readSecret = null,
            writeSecret = null,
            shareToken = token,
        )
        val events = repoB.subscribe()

        sessionA.bindNetwork(quicV4 = "127.0.0.1:0")
        sessionB.bindNetwork(quicV4 = "127.0.0.1:0")

        val addrA = sessionA.quicListenerLocalAddrV4()!!
        sessionB.addUserProvidedPeer("quic/$addrA")

        repoA.setSyncEnabled(true)
        repoB.setSyncEnabled(true)

        val contentA = "hello world"
        val fileA = File.create(repoA, "test.txt")
        fileA.write(0, contentA.toByteArray())
        fileA.close()

        while (true) {
            try {
                val fileB = File.open(repoB, "test.txt")
                try {
                    val length = fileB.length()
                    val contentB = fileB.read(0, length).decodeToString()

                    if (contentB == contentA) {
                        break
                    }
                } finally {
                    fileB.close()
                }
            } catch (e: Exception) {
            }

            events.receive()
        }

        events.close()
        repoA.close()
        repoB.close()
    }
}
