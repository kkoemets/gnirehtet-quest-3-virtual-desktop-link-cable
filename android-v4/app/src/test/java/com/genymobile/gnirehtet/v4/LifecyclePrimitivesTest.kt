package com.genymobile.gnirehtet.v4

import java.util.concurrent.CountDownLatch
import java.util.concurrent.Executors
import java.util.concurrent.TimeUnit
import java.util.concurrent.atomic.AtomicInteger
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test

class LifecyclePrimitivesTest {
    @Test
    fun ignoresCallbacksFromInvalidatedGeneration() {
        val gate = GenerationGate()
        val first = gate.begin()
        assertTrue(gate.isCurrent(first))
        assertTrue(gate.invalidate(first))
        assertFalse(gate.isCurrent(first))
        val second = gate.begin()
        assertTrue(gate.isCurrent(second))
        assertFalse(gate.isCurrent(first))
    }

    @Test
    fun acknowledgesConcurrentStopWaitersExactlyOnceAfterDescriptorClose() {
        val acknowledgements = StopAcknowledgements()
        val executor = Executors.newFixedThreadPool(8)
        val start = CountDownLatch(1)
        val completed = CountDownLatch(WAITER_COUNT)
        val invocationCount = AtomicInteger()

        repeat(WAITER_COUNT) {
            executor.execute {
                start.await()
                val waiter = {
                    invocationCount.incrementAndGet()
                    completed.countDown()
                }
                when (acknowledgements.register(waiter)) {
                    StopAcknowledgements.Registration.QUEUED -> Unit
                    StopAcknowledgements.Registration.RUN_NOW -> waiter()
                    StopAcknowledgements.Registration.REJECTED -> throw AssertionError("descriptor close failed")
                }
            }
        }
        start.countDown()
        executor.execute {
            acknowledgements.descriptorClosed().forEach { it() }
        }

        assertTrue(completed.await(5, TimeUnit.SECONDS))
        assertEquals(WAITER_COUNT, invocationCount.get())
        executor.shutdownNow()
    }

    @Test
    fun descriptorFailureRejectsStopAcknowledgement() {
        val acknowledgements = StopAcknowledgements()
        acknowledgements.descriptorCloseFailed()
        assertEquals(
            StopAcknowledgements.Registration.REJECTED,
            acknowledgements.register { throw AssertionError("must not run") },
        )
    }

    @Test
    fun preActiveFailureStillTargetsTheCurrentGeneration() {
        assertTrue(
            stopTargetsGeneration(
                expectedGeneration = 7,
                activeGeneration = null,
                teardownInProgress = false,
                closingGeneration = 0,
                expectedIsCurrent = true,
            ),
        )
    }

    @Test
    fun staleFailureCannotStopAnotherGeneration() {
        assertFalse(
            stopTargetsGeneration(
                expectedGeneration = 7,
                activeGeneration = 8,
                teardownInProgress = false,
                closingGeneration = 0,
                expectedIsCurrent = false,
            ),
        )
    }

    @Test
    fun malformedStartOnlyFinishesAnIdleService() {
        assertTrue(canFinishRejectedStart(hasActiveResources = false, teardownInProgress = false))
        assertFalse(canFinishRejectedStart(hasActiveResources = true, teardownInProgress = false))
        assertFalse(canFinishRejectedStart(hasActiveResources = false, teardownInProgress = true))
    }

    @Test
    fun transportEngineResetIsRequiredOncePerConfirmedLoss() {
        val reset = TransportEngineReset()

        assertFalse(reset.isRequired())
        assertTrue(reset.require())
        assertFalse(reset.require())
        assertTrue(reset.isRequired())
        assertFalse(reset.awaitReady(1))
        assertTrue(reset.complete())
        assertFalse(reset.complete())
        assertFalse(reset.isRequired())
        assertTrue(reset.awaitReady(1))
        assertTrue(reset.require())
    }

    companion object {
        private const val WAITER_COUNT = 200
    }
}
