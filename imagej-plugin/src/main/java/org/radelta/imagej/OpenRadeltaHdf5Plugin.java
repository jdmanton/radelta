package org.radelta.imagej;

import ij.IJ;
import ij.plugin.PlugIn;
import java.io.IOException;

/** Also works in ImageJ1, where no SciJava startup service is created. */
public class OpenRadeltaHdf5Plugin implements PlugIn {
    @Override
    public void run(String argument) {
        try {
            RadeltaHdf5.configure();
            IJ.runPlugIn("sc.fiji.hdf5.HDF5_Reader_Vibez", argument == null ? "" : argument);
        } catch (IOException e) {
            IJ.error("Radelta HDF5", e.getMessage());
        }
    }
}
