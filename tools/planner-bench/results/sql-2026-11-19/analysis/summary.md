# Planner bench (SQL)

1570 jobs; outcomes: {'ok': 1524, 'killed_timeout': 45, 'killed_memory': 1}

## Scaling: growth exponent (log-log slope vs queries)

| series | max N ok | S0–2 ms | S3 ms | peak RSS S0–2 | peak RSS S3 | logical cands | MILP vars |
|---|---|---|---|---|---|---|---|
| scale_baseline | 1024 | 1.46 | 1.68 | 0.26 | 0.43 | 1.35 | 1.18 |
| scale_high_sharing | 1024 | 1.43 | 1.66 | 0.28 | 0.46 | 1.25 | 1.13 |
| scale_low_sharing | 512 | 1.31 | 1.59 | 0.22 | 0.42 | 1.21 | 1.21 |
| scale_only_count_distinct | 1024 | 1.22 | 1.21 | 0.33 | 0.35 | 1.18 | 0.96 |
| scale_only_quantile | 256 | 1.12 | 1.72 | 0.13 | 0.49 | 1.08 | 1.08 |
| scale_only_sum | 1024 | 1.05 | 1.44 | 0.16 | 0.41 | 1.05 | 1.06 |
| scale_only_topk | 1024 | 1.25 | 1.31 | 0.50 | 0.51 | 1.24 | 1.04 |

First non-ok rank per scaling series:

- scale_baseline: N=1024 → killed_timeout
- scale_high_sharing: N=1024 → killed_timeout
- scale_low_sharing: N=1024 → killed_timeout
- scale_only_count_distinct: N=1024 → killed_timeout
- scale_only_quantile: N=512 → killed_timeout

## One factor at a time (median over seeds)

### accuracy (N=16)

```
cfg_accuracy  queries  s02_ms   s3_ms  rss02_mib  rss3_mib  logical  milp_vars
     uniform     16.0   127.2   844.3       54.3      65.9   1044.0     1384.0
       mixed     16.0   119.5   793.2       54.2      66.5   1053.0     1374.0
       exact     16.0   145.9   196.5       54.4      60.1    969.0      574.0
      ignore     16.0   328.9 14992.6       60.0     274.8   3249.0    13160.0
```

### accuracy (N=64)

```
cfg_accuracy  queries  s02_ms     s3_ms  rss02_mib  rss3_mib  logical  milp_vars
     uniform     64.0   895.7   13757.7       67.6     135.4   6133.0     6062.0
       mixed     64.0   822.5   12416.5       68.1     145.5   6299.0     7312.0
       exact     64.0  1005.4    1281.8       67.5      83.5   5828.0     1959.0
      ignore     64.0  2015.4 2274295.9       93.0    1491.8  16998.0    93688.5
```

### agg_mix (N=16)

```
                                       cfg_agg_mix  queries  s02_ms   s3_ms  rss02_mib  rss3_mib  logical  milp_vars
                                               sum     16.0     7.3   925.4       50.4      61.7    187.0     1120.0
                                             count     16.0     2.9   105.7       51.0      55.4     59.0      307.0
                                          quantile     16.0    30.6 12789.1       53.2     123.6    959.0     4525.0
                                    count_distinct     16.0    71.3    24.1       56.2      58.7   2004.0      191.0
                                              topk     16.0  1038.4   343.0       62.2      67.8   4615.0      594.0
                                               avg     16.0     2.2    20.4       49.7      52.6     36.0      191.0
                                               min     16.0     9.4  1247.7       51.4      62.1    187.0     1112.0
                                               max     16.0     7.7  1024.8       50.7      61.9    187.0     1112.0
sum,count,quantile,count_distinct,topk,avg,min,max     16.0    23.9   298.9       53.6      61.7    652.0      869.0
```

### agg_mix (N=64)

```
                                       cfg_agg_mix  queries  s02_ms    s3_ms  rss02_mib  rss3_mib  logical  milp_vars
                                               sum     63.0    30.7   5663.0       54.8      96.0    785.0     4847.0
                                             count     63.0    13.5    493.2       51.9      63.4    286.0     1528.0
                                          quantile     64.0    90.3 117339.9       61.1     262.2   2767.0    14896.0
                                    count_distinct     63.0   455.8     63.2       83.7      86.0  11020.0      661.0
                                              topk     64.0  3820.1   1926.9       89.1     105.9  15140.0     2423.0
                                               avg     63.0     9.3     43.5       51.1      54.5    130.0      661.0
                                               min     63.0    32.1   6556.6       54.3      95.1    785.0     4847.0
                                               max     63.0    32.7   6573.9       54.5      95.4    785.0     4847.0
sum,count,quantile,count_distinct,topk,avg,min,max     64.0   438.3   7057.7       61.2      93.3   3269.0     3998.0
```

### aggs_per_query (N=16)

```
 cfg_aggs_per_query  queries  s02_ms  s3_ms  rss02_mib  rss3_mib  logical  milp_vars
                  1     16.0   155.5 1046.3       53.9      65.3   1044.0     1384.0
                  2     16.0   142.7  527.8       56.7      64.9   1799.0      885.0
                  3     16.0   375.3  970.9       59.4      72.5   2797.0     1275.0
                  4     16.0   195.2 1314.3       60.0      74.3   2579.0     1555.0
```

### aggs_per_query (N=64)

```
 cfg_aggs_per_query  queries  s02_ms   s3_ms  rss02_mib  rss3_mib  logical  milp_vars
                  1     64.0  1011.9 16409.4       67.3     136.1   6133.0     6062.0
                  2     64.0   691.1  3032.1       71.6     102.7   7263.0     3393.0
                  3     64.0  1151.7  4827.4       81.5     122.8   9785.0     4511.0
                  4     64.0   981.2  6269.9       83.1     136.3  10063.0     5415.0
```

### capabilities (N=16)

```
cfg_capabilities  queries  s02_ms  s3_ms  rss02_mib  rss3_mib  logical  milp_vars
           every     16.0   148.0 1029.4       54.0      65.4   1044.0     1384.0
         default     16.0   120.5  842.9       54.1      65.1   1044.0     1384.0
```

### capabilities (N=64)

```
cfg_capabilities  queries  s02_ms   s3_ms  rss02_mib  rss3_mib  logical  milp_vars
           every     64.0   860.8 13692.1       67.4     135.6   6133.0     6062.0
         default     64.0   812.4 13552.9       67.2     135.5   6133.0     6062.0
```

### filters (N=16)

```
 cfg_filter_frac cfg_filter_kind  cfg_filter_values  queries  s02_ms  s3_ms  rss02_mib  rss3_mib  logical  milp_vars
             0.0              eq                  1     16.0   147.8 1042.5       54.1      65.5   1044.0     1384.0
             0.5              eq                  1     16.0   242.8  670.6       55.6      72.7   1609.0     1706.0
             1.0              in                 16     16.0   115.9  974.3       54.1      68.0    790.0     1370.0
             0.5              eq                  4     16.0   242.4  648.2       56.1      72.6   1609.0     1712.0
             0.5              eq                 16     16.0   301.3  906.3       56.0      72.8   1611.0     1712.0
             0.5              in                  4     16.0   242.7 2435.5       55.2      70.7   1495.0     1862.0
             0.5              in                 16     16.0   243.9 3779.9       55.7      70.5   1454.0     1718.0
             1.0              eq                  1     16.0   134.6 1377.9       54.3      74.8    935.0     1249.0
             1.0              eq                  4     16.0   174.8 2582.4       54.8      70.5    946.0     1221.0
             1.0              eq                 16     16.0   135.9 2203.7       54.7      73.3    948.0     1248.0
             1.0              in                  4     16.0   136.2 3968.2       55.0      78.7    961.0     1401.0
```

### filters (N=64)

```
 cfg_filter_frac cfg_filter_kind  cfg_filter_values  queries  s02_ms   s3_ms  rss02_mib  rss3_mib  logical  milp_vars
             0.0              eq                  1     64.0   832.1 13631.9       67.3     135.4   6133.0     6062.0
             0.5              eq                  1     64.0  1363.0 21389.1       70.7     134.5   7677.0     5641.0
             1.0              in                 16     64.0   660.2 20793.7       66.1     120.3   4280.0     5026.0
             0.5              eq                  4     64.0  1068.4 20115.6       71.7     131.0   7714.0     5835.0
             0.5              eq                 16     64.0  1140.9 25778.5       71.3     131.1   7740.0     5951.0
             0.5              in                  4     64.0  1368.9 33219.0       72.3     142.4   7755.0     6604.0
             0.5              in                 16     64.0   739.4 24860.3       69.5     140.2   5973.0     6711.0
             1.0              eq                  1     64.0   673.8 17072.4       64.8     110.7   4556.0     4213.0
             1.0              eq                  4     64.0   718.6 27585.0       65.3     111.1   4573.0     4352.0
             1.0              eq                 16     64.0   863.2 26054.0       65.9     110.5   4615.0     4648.0
             1.0              in                  4     64.0   771.9 31795.2       65.7     124.8   4789.0     5168.0
```

### gb (N=16)

```
 cfg_gb_width cfg_gb_structure  queries  s02_ms  s3_ms  rss02_mib  rss3_mib  logical  milp_vars
            0             same     16.0   311.9  624.3       55.1      66.1   1429.0      923.0
            1             same     16.0   424.8  599.0       55.2      65.0   1445.0      917.0
            4             same     16.0   615.8  560.7       55.0      62.9   1412.0      769.0
            4           nested     16.0   152.7  933.0       54.6      66.9   1321.0      920.0
            4           random     16.0   123.0 1007.8       54.4      70.7   1141.0     1519.0
            5             same     16.0   526.3  202.1       55.9      62.6   1389.0      850.0
            5           nested     16.0   247.6  963.3       55.1      66.4   1236.0      756.0
            5           random     16.0   184.8 1249.5       55.3      67.7   1141.0     1386.0
            6             same     16.0   733.3  265.6       56.1      63.3   1398.0      850.0
            6           nested     16.0   419.3 1255.5       56.3      69.6   1654.0      934.0
            6           random     16.0   621.1  535.4       57.4      67.9   2206.0     1142.0
            1           nested     16.0   426.6  586.8       55.8      65.0   1445.0      917.0
            1           random     16.0   126.6  968.2       54.7      67.0   1140.0     1073.0
            2             same     16.0   382.3  683.8       55.5      65.1   1418.0      917.0
            2           nested     16.0   277.6 1023.1       55.2      66.1   1620.0      995.0
            2           random     16.0   121.9  831.4       54.5      66.1   1044.0     1384.0
            3             same     16.0   416.2  413.0       55.2      63.4   1438.0      850.0
            3           nested     16.0   218.9  466.7       54.5      66.2   1293.0      878.0
            3           random     16.0   370.3  548.5       56.5      66.7   1837.0     1307.0
```

### gb (N=64)

```
 cfg_gb_width cfg_gb_structure  queries  s02_ms   s3_ms  rss02_mib  rss3_mib  logical  milp_vars
            0             same     64.0  2467.1  1963.8       73.6      90.0   8516.0     2636.0
            1             same     39.0   992.6   936.0       58.4      71.4   2109.0     1730.0
            4             same     39.0  1501.4   996.0       59.5      75.2   2102.0     1643.0
            4           nested     64.0  1796.9  9682.2       68.8     119.6   6115.0     4673.0
            4           random     64.0   769.6 11925.3       67.5     130.9   5828.0     6170.0
            5             same     39.0  1635.5  1124.1       59.3      75.8   2102.0     1643.0
            5           nested     64.0  1131.8 12617.3       67.1     128.3   5704.0     5159.0
            5           random     64.0   934.5  8804.9       67.2     119.0   5501.0     4369.0
            6             same     39.0  1746.6   816.8       59.9      72.9   2102.0     1643.0
            6           nested     64.0  1447.1 11848.2       69.2     123.0   6157.0     5165.0
            6           random     64.0  1556.1  7792.7       72.0     125.6   7360.0     4976.0
            1           nested     39.0  1154.5  1123.9       58.2      71.7   2109.0     1730.0
            1           random     64.0   914.5 12781.6       65.9     119.1   5464.0     4860.0
            2             same     39.0  1210.1  1815.8       59.1      76.7   2109.0     1742.0
            2           nested     64.0  1609.3  4520.2       64.3      94.2   4349.0     3217.0
            2           random     64.0   990.8 16349.9       67.5     135.4   6133.0     6062.0
            3             same     39.0  1253.7  1562.5       59.0      77.7   2102.0     1643.0
            3           nested     64.0  1419.1  8800.7       66.3     121.5   5542.0     3740.0
            3           random     64.0  1138.8 11897.7       68.6     130.0   6804.0     5762.0
```

### intervals (N=16)

```
 cfg_n_intervals cfg_interval_pool cfg_phases  queries  s02_ms  s3_ms  rss02_mib  rss3_mib  logical  milp_vars
               1           aligned       zero     16.0   150.4 1052.6       54.3      65.7   1044.0     1384.0
               1           aligned     random     16.0   287.6  668.5       55.7      67.9   1650.0     1427.0
               4           coprime       zero     16.0   240.5 1034.7       55.2      71.6   1502.0     1455.0
               4           coprime     random     16.0   237.3  482.8       54.7      67.6   1477.0     1248.0
               1           coprime       zero     16.0   145.7  638.3       54.1      65.0   1044.0     1357.0
               1           coprime     random     16.0   256.1 1240.1       55.8      70.3   1426.0     1739.0
               2           aligned       zero     16.0   237.9 1342.8       55.4      70.0   1476.0     1208.0
               2           aligned     random     16.0   250.1 3408.4       55.0      74.8   1095.0     1523.0
               2           coprime       zero     16.0   295.3 1009.3       54.5      66.6   1448.0     1200.0
               2           coprime     random     16.0   290.3 2150.9       54.7      71.4   1426.0     1586.0
               4           aligned       zero     16.0   236.2  891.5       55.8      73.7   1509.0     1465.0
               4           aligned     random     16.0   257.9  661.8       54.6      64.5   1386.0      968.0
```

### intervals (N=64)

```
 cfg_n_intervals cfg_interval_pool cfg_phases  queries  s02_ms   s3_ms  rss02_mib  rss3_mib  logical  milp_vars
               1           aligned       zero     64.0  1059.9 16388.6       67.2     135.6   6133.0     6062.0
               1           aligned     random     64.0   898.0 17765.6       67.8     137.9   6304.0     7700.0
               4           coprime       zero     64.0   935.0 11299.4       67.1     127.6   6512.0     7940.0
               4           coprime     random     64.0  1053.5  9709.1       68.9     119.3   7004.0     6241.0
               1           coprime       zero     64.0   991.2  9484.9       67.3     124.0   6133.0     6062.0
               1           coprime     random     64.0  1012.6  8059.2       67.9     113.5   6540.0     5693.0
               2           aligned       zero     64.0  1019.7 12498.1       68.8     132.9   6650.0     7015.0
               2           aligned     random     64.0  1066.9 17757.9       69.7     138.3   6874.0     7995.0
               2           coprime       zero     64.0  1340.0  9878.2       67.9     125.6   6515.0     6916.0
               2           coprime     random     64.0   992.4  7924.3       67.5     112.5   6540.0     5580.0
               4           aligned       zero     64.0   936.4 15596.7       67.9     139.6   6759.0     8046.0
               4           aligned     random     64.0   928.0 10589.1       68.1     133.6   6470.0     6793.0
```

### tables (N=16)

```
 cfg_n_tables  queries  s02_ms  s3_ms  rss02_mib  rss3_mib  logical  milp_vars
            1     16.0   149.1 1035.9       53.8      65.5   1044.0     1384.0
            2     16.0   118.1 2091.0       54.3      68.4    995.0     1147.0
            4     16.0   112.8 1373.8       54.1      68.1    937.0     1193.0
```

### tables (N=64)

```
 cfg_n_tables  queries  s02_ms   s3_ms  rss02_mib  rss3_mib  logical  milp_vars
            1     64.0   807.9 13503.6       67.8     135.5   6133.0     6062.0
            2     64.0   996.6 11690.0       67.7     140.4   5942.0     5981.0
            4     64.0   851.1  8673.1       67.0     135.4   5964.0     5944.0
```

### windows (N=16)

```
 cfg_n_windows cfg_window_pool  queries  s02_ms  s3_ms  rss02_mib  rss3_mib  logical  milp_vars
             1          nested     16.0   264.3  611.1       55.5      66.8   1832.0     1352.0
             1         coprime     16.0   260.8  632.1       55.5      65.6   1832.0     1376.0
             2          nested     16.0   311.8  995.8       55.1      63.4   1504.0     1052.0
             2         coprime     16.0   256.7  941.5       55.4      66.6   1504.0     1047.0
             4          nested     16.0   248.8 2768.3       56.2      65.1   1674.0     1001.0
             4         coprime     16.0   244.9  699.0       55.3      62.9   1674.0      996.0
             7          nested     16.0   271.3 2381.8       54.8      67.2   1383.0     1146.0
             7         coprime     16.0   218.9  920.7       55.3      64.9   1383.0     1138.0
```

### windows (N=64)

```
 cfg_n_windows cfg_window_pool  queries  s02_ms   s3_ms  rss02_mib  rss3_mib  logical  milp_vars
             1          nested     64.0  1352.3 13812.4       72.9     178.4   8111.0     5740.0
             1         coprime     64.0  1376.6 11020.7       72.5     149.8   8111.0     5740.0
             2          nested     64.0  1138.6 13044.6       67.3     134.2   5992.0     6185.0
             2         coprime     64.0   946.1 10822.7       67.2     136.1   5992.0     6185.0
             4          nested     64.0  1114.6 25290.7       69.9     127.9   7225.0     5744.0
             4         coprime     64.0  1098.4 18100.6       69.4     138.2   7225.0     5744.0
             7          nested     64.0   989.9 24436.7       66.1     123.5   5314.0     6068.0
             7         coprime     64.0   682.4 14957.9       66.0     135.3   5314.0     6023.0
```

## log-log regression (all ok jobs)

- **stage0_2_ms** correlation of logs: logical_candidates 0.94, memo_groups 0.70, queries 0.68, milp_variables 0.64, physical_candidates 0.63, distinct_group_bys 0.36, filtered_queries 0.28, distinct_windows 0.08, distinct_schedules 0.08
- **stage3_ms** correlation of logs: milp_variables 0.94, physical_candidates 0.93, memo_groups 0.82, queries 0.75, logical_candidates 0.72, filtered_queries 0.45, distinct_group_bys 0.41, distinct_windows 0.16, distinct_schedules 0.15
- **peak_rss_stage0_2_bytes** correlation of logs: logical_candidates 0.74, memo_groups 0.72, queries 0.68, physical_candidates 0.60, milp_variables 0.60, filtered_queries 0.53, distinct_group_bys 0.33, distinct_schedules 0.07, distinct_windows 0.07
- **peak_rss_stage3_bytes** correlation of logs: milp_variables 0.87, physical_candidates 0.87, memo_groups 0.76, logical_candidates 0.73, queries 0.72, filtered_queries 0.48, distinct_group_bys 0.35, distinct_schedules 0.14, distinct_windows 0.08

Stage 3 share of planning time: median 95%, p90 100%
