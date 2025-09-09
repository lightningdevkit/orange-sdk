package org.lightningdevkit.orangesdk

import org.junit.jupiter.api.BeforeAll
import org.junit.jupiter.api.Test
import org.junit.jupiter.api.TestInstance
import java.net.URI
import java.net.http.HttpClient
import java.net.http.HttpRequest
import java.net.http.HttpResponse
import kotlin.io.path.createTempDirectory
import kotlin.test.assertEquals
import kotlin.test.assertTrue

fun runCommandAndWait(vararg cmd: String): String {
    println("Running command \"${cmd.joinToString(" ")}\"")

    val processBuilder = ProcessBuilder(cmd.toList())
    val process = processBuilder.start()

    process.waitFor()
    val stdout = process.inputStream.bufferedReader().lineSequence().joinToString("\n")
    val stderr = process.errorStream.bufferedReader().lineSequence().joinToString("\n")
    return stdout + stderr
}

fun bitcoinCli(vararg cmd: String): String {
    val bitcoinCliBin = System.getenv("BITCOIN_CLI_BIN")?.split(" ") ?: listOf("bitcoin-cli")
    val bitcoinDRpcUser = System.getenv("BITCOIND_RPC_USER") ?: ""
    val bitcoinDRpcPassword = System.getenv("BITCOIND_RPC_PASSWORD") ?: ""

    val baseCommand = bitcoinCliBin + "-regtest"

    val rpcAuth = if (bitcoinDRpcUser.isNotBlank() && bitcoinDRpcPassword.isNotBlank()) {
        listOf("-rpcuser=$bitcoinDRpcUser", "-rpcpassword=$bitcoinDRpcPassword")
    } else {
        emptyList()
    }

    val fullCommand = baseCommand + rpcAuth + cmd.toList()
    return runCommandAndWait(*fullCommand.toTypedArray())
}

fun mine(blocks: UInt): String {
    val address = bitcoinCli("getnewaddress")
    val output = bitcoinCli("generatetoaddress", blocks.toString(), address)
    println("Mining output: $output")
    val re = Regex("\n.+\n\\]$")
    val lastBlock = re.find(output)!!.value.replace("]", "").replace("\"", "").replace("\n", "").trim()
    println("Last block: $lastBlock")
    return lastBlock
}

fun mineAndWait(esploraEndpoint: String, blocks: UInt) {
    val lastBlockHash = mine(blocks)
    waitForBlock(esploraEndpoint, lastBlockHash)
}

fun sendToAddress(address: String, amountSats: UInt): String {
    val amountBtc = amountSats.toDouble() / 100000000.0
    val output = bitcoinCli("sendtoaddress", address, amountBtc.toString())
    return output
}

fun waitForTx(esploraEndpoint: String, txid: String) {
    var esploraPickedUpTx = false
    val re = Regex("\"txid\":\"$txid\"")
    while (!esploraPickedUpTx) {
        val client = HttpClient.newBuilder().build()
        val request = HttpRequest.newBuilder()
            .uri(URI.create(esploraEndpoint + "/tx/" + txid))
            .build()

        val response = client.send(request, HttpResponse.BodyHandlers.ofString())

        esploraPickedUpTx = re.containsMatchIn(response.body())
        Thread.sleep(500)
    }
}

fun waitForBlock(esploraEndpoint: String, blockHash: String) {
    var esploraPickedUpBlock = false
    val re = Regex("\"in_best_chain\":true")
    while (!esploraPickedUpBlock) {
        val client = HttpClient.newBuilder().build()
        val request = HttpRequest.newBuilder()
            .uri(URI.create(esploraEndpoint + "/block/" + blockHash + "/status"))
            .build()

        val response = client.send(request, HttpResponse.BodyHandlers.ofString())

        esploraPickedUpBlock = re.containsMatchIn(response.body())
        Thread.sleep(500)
    }
}


@TestInstance(TestInstance.Lifecycle.PER_CLASS)
class LibraryTest {

    val esploraEndpoint = System.getenv("ESPLORA_ENDPOINT")

    @BeforeAll
    fun setup() {
        bitcoinCli("createwallet", "orange_sdk_test")
        bitcoinCli("loadwallet", "orange_sdk_test", "true")
        mine(101u)
        Thread.sleep(5_000)
    }

    @Test fun fullCycle() {
        println("do full test cycle")
    }
}