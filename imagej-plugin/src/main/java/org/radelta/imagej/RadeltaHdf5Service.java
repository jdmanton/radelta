package org.radelta.imagej;

import java.io.IOException;
import org.scijava.log.LogService;
import org.scijava.plugin.Parameter;
import org.scijava.plugin.Plugin;
import org.scijava.service.AbstractService;
import org.scijava.service.Service;

/** Configure Fiji HDF5 import commands when its context starts. */
@Plugin(type = Service.class)
public class RadeltaHdf5Service extends AbstractService {
    @Parameter(required = false)
    private LogService log;

    @Override
    public void initialize() {
        // A standalone .rdlt installation need not install HDF5 dependencies.
        if (!RadeltaHdf5.hasReader()) return;
        try {
            RadeltaHdf5.configure();
        } catch (IOException e) {
            // HDF5 availability must not prevent Fiji or the Radelta reader
            // from starting. The HDF5 menu command reports the actionable error.
            if (log != null) log.warn("Radelta HDF5: " + e.getMessage());
        }
    }
}
