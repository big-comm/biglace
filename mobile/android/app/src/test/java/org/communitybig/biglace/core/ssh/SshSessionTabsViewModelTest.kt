package org.communitybig.biglace.core.ssh

import org.junit.Assert.assertEquals
import org.junit.Assert.assertNotEquals
import org.junit.Test

class SshSessionTabsViewModelTest {
    @Test
    fun peerTargetsGetIndependentTabsAndExistingTargetIsReused() {
        val vm = SshSessionTabsViewModel()

        val first = vm.openPeer("server-a", "alice", "one")
        val second = vm.openPeer("server-b", "bob", "two")

        assertNotEquals(first, second)
        assertEquals(2, vm.tabs.value.size)
        assertEquals(second, vm.activeId.value)
        assertEquals(first, vm.openPeer("SERVER-A", "alice", "changed"))
        assertEquals(2, vm.tabs.value.size)
    }

    @Test
    fun closingActiveTabSelectsNeighborAndKeepsOneBlankTab() {
        val vm = SshSessionTabsViewModel()
        val first = vm.openPeer("server-a", "alice", "")
        val second = vm.openPeer("server-b", "bob", "")

        vm.close(second)
        assertEquals(first, vm.activeId.value)

        vm.close(first)
        assertEquals(1, vm.tabs.value.size)
        assertEquals("", vm.tabs.value.single().host)
        assertEquals(vm.tabs.value.single().id, vm.activeId.value)
    }
}
