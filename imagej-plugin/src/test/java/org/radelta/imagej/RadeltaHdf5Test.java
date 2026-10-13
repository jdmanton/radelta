package org.radelta.imagej;

import org.junit.Test;
import static org.junit.Assert.*;

public class RadeltaHdf5Test {
    @Test
    public void selectsTheJavaRuntimeArchitecture() {
        assertEquals("/hdf5/windows-x86_64/H5Zradelta.dll", RadeltaHdf5.nativeResource("Windows 11", "amd64"));
        assertEquals("/hdf5/macos-x86_64/libH5Zradelta.dylib", RadeltaHdf5.nativeResource("Mac OS X", "x86_64"));
        assertEquals("/hdf5/macos-aarch64/libH5Zradelta.dylib", RadeltaHdf5.nativeResource("Mac OS X", "aarch64"));
        assertEquals("/hdf5/macos-aarch64/libH5Zradelta.dylib", RadeltaHdf5.nativeResource("Darwin", "arm64"));
        assertEquals("/hdf5/linux-aarch64/libH5Zradelta.so", RadeltaHdf5.nativeResource("Linux", "aarch64"));
        assertEquals("/hdf5/linux-x86_64/libH5Zradelta.so", RadeltaHdf5.nativeResource("Linux", "x86_64"));
        assertNull(RadeltaHdf5.nativeResource("Windows 11", "aarch64"));
        assertNull(RadeltaHdf5.nativeResource("Linux", "arm"));
        assertNull(RadeltaHdf5.nativeResource("Linux", "x86"));
    }
}
