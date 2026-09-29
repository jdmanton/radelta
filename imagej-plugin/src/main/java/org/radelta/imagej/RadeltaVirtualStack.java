package org.radelta.imagej;

import ij.IJ;
import ij.VirtualStack;
import ij.process.ImageProcessor;
import ij.process.ShortProcessor;

import java.io.IOException;

final class RadeltaVirtualStack extends VirtualStack {
    private final RadeltaFile file;
    private final int size;
    private String[] metadataLabels;

    void setMetadataLabels(String[] labels) { metadataLabels = labels; }

    RadeltaVirtualStack(RadeltaFile file) {
        super(file.width, file.height, file.planeCount());
        this.file = file;
        this.size = file.planeCount();
        setBitDepth(16);
    }

    RadeltaFile getRadeltaFile() {
        return file;
    }

    @Override
    public int getSize() {
        return size;
    }

    @Override
    public int getBitDepth() {
        return 16;
    }

    @Override
    public synchronized ImageProcessor getProcessor(int n) {
        if (n < 1 || n > size) throw new IllegalArgumentException("Slice out of range: " + n);
        int i = n - 1;
        // ImageJ hyperstack order is C fastest, then Z, then T.
        int c = i % file.channels;
        int z = (i / file.channels) % file.slices;
        int t = i / (file.channels * file.slices);
        try {
            short[] pixels = file.readPlane(t, c, z);
            ShortProcessor ip = new ShortProcessor(file.width, file.height, pixels, null);
            ip.setSliceNumber(n);
            return ip;
        } catch (IOException e) {
            IJ.handleException(e);
            return new ShortProcessor(file.width, file.height);
        }
    }

    @Override
    public String getSliceLabel(int n) {
        if (n < 1 || n > size) return null;
        if (metadataLabels != null) return metadataLabels[n - 1];
        int i = n - 1;
        int c = i % file.channels;
        int z = (i / file.channels) % file.slices;
        int t = i / (file.channels * file.slices);
        return "C=" + (c + 1) + " Z=" + (z + 1) + " T=" + (t + 1);
    }

    @Override
    public synchronized void setSliceLabel(String label, int n) {
        if (n < 1 || n > size) throw new IllegalArgumentException("Slice out of range: " + n);
        if (metadataLabels == null) {
            String[] labels = new String[size];
            for (int i = 1; i <= size; i++) labels[i - 1] = getSliceLabel(i);
            metadataLabels = labels;
        }
        metadataLabels[n - 1] = label;
    }

    void close() {
        file.close();
    }
}
