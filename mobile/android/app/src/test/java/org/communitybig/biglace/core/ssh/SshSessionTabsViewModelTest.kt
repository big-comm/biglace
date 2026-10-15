package org.communitybig.biglace.core.ssh

import org.junit.Assert.assertEquals
import org.junit.Assert.assertNotEquals
import org.junit.Assert.assertNotSame
import org.junit.Assert.assertSame
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

    @Test
    fun closingATabReleasesItsViewModelStore() {
        val vm = SshSessionTabsViewModel()
        val id = vm.openPeer("server-a", "alice", "")
        val owner = vm.ownerFor(id)
        assertSame(owner, vm.ownerFor(id))

        vm.close(id)

        assertNotSame(owner, vm.ownerFor(id))
    }

    @Test
    fun duplicatingASessionOpensAConnectingCopyNextToIt() {
        val vm = SshSessionTabsViewModel()
        val first = vm.openPeer("100.64.0.5", "tales", "secret")
        val other = vm.openPeer("100.64.0.9", "maria", "")

        val copy = vm.duplicate(first)!!

        val tabs = vm.tabs.value
        assertEquals(listOf(first, copy, other), tabs.map { it.id })
        val source = tabs[0]
        val dup = tabs[1]
        assertEquals(copy, vm.activeId.value)
        assertEquals(source.host, dup.host)
        assertEquals(source.user, dup.user)
        assertEquals(source.password, dup.password)
        assertEquals(source.port, dup.port)
        assertEquals(true, dup.autoConnect)
        assertEquals("tales@100.64.0.5", dup.label)
    }

    @Test
    fun aBlankTabCannotBeDuplicated() {
        val vm = SshSessionTabsViewModel()
        assertEquals(null, vm.duplicate(vm.activeId.value))
        assertEquals(1, vm.tabs.value.size)
    }
}
