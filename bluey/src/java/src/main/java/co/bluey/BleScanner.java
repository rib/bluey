package co.bluey;

import android.content.Context;

import java.io.Closeable;
import java.io.IOException;

public class BleScanner implements Closeable {
    private Context ctx;
    private BleSession session;

    public BleScanner(BleSession session) {
        this.ctx = session.context();
        this.session = session;
    }

    @Override
    public void close() throws IOException {

    }
}
