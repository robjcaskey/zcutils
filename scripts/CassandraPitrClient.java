import com.datastax.driver.core.Cluster;
import com.datastax.driver.core.ColumnDefinitions;
import com.datastax.driver.core.ConsistencyLevel;
import com.datastax.driver.core.ResultSet;
import com.datastax.driver.core.Row;
import com.datastax.driver.core.Session;
import com.datastax.driver.core.SimpleStatement;

public final class CassandraPitrClient {
    public static void main(String[] args) {
        if (args.length != 2) {
            System.err.println("usage: CassandraPitrClient CONTACT QUERY");
            System.exit(2);
        }
        try (Cluster cluster = Cluster.builder().addContactPoint(args[0]).withPort(9042).build();
             Session session = cluster.connect()) {
            SimpleStatement statement = new SimpleStatement(args[1]);
            statement.setConsistencyLevel(ConsistencyLevel.ALL);
            ResultSet result = session.execute(statement);
            for (Row row : result) {
                StringBuilder line = new StringBuilder();
                for (ColumnDefinitions.Definition column : row.getColumnDefinitions()) {
                    if (line.length() != 0) line.append(' ');
                    String name = column.getName();
                    line.append(name).append('=').append(row.getObject(name));
                }
                System.out.println(line);
            }
        }
    }
}
